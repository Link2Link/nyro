#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Prefer the static musl build (`make server-musl`): it does not depend on the
# runtime image's glibc and matches the binaries that in-place updates install.
MUSL_BINARY="${ROOT_DIR}/target/x86_64-unknown-linux-musl/release/nyro-server"
GNU_BINARY="${ROOT_DIR}/target/release/nyro-server"
if [[ -n "${NYRO_SERVER_BINARY:-}" ]]; then
    SERVER_BINARY="${NYRO_SERVER_BINARY}"
elif [[ -x "${MUSL_BINARY}" ]]; then
    SERVER_BINARY="${MUSL_BINARY}"
else
    SERVER_BINARY="${GNU_BINARY}"
fi
IMAGE_TAG="nyro:mine"
OUTPUT_ARCHIVE="${ROOT_DIR}/nyro-mine.tar"
ENV_FILE="${ROOT_DIR}/.env"
RUNTIME_IMAGE="${RUNTIME_IMAGE:-debian:bookworm-slim}"
# Docker Hub is unreliable from mainland China; these registry mirrors are tried
# in order when the base image is not cached and the Docker Hub pull fails.
DOCKER_MIRRORS="${NYRO_DOCKER_MIRRORS:-docker.m.daocloud.io docker.1ms.run}"
BUILD_CONTEXT=""
TEMP_ARCHIVE=""

die() {
    echo "error: $*" >&2
    exit 1
}

cleanup() {
    if [[ -n "${BUILD_CONTEXT}" ]]; then
        rm -rf -- "${BUILD_CONTEXT}"
    fi
    if [[ -n "${TEMP_ARCHIVE}" ]]; then
        rm -f -- "${TEMP_ARCHIVE}"
    fi
}

generate_admin_token() {
    if command -v openssl >/dev/null 2>&1; then
        openssl rand -hex 32
    elif command -v od >/dev/null 2>&1; then
        od -An -N32 -tx1 /dev/urandom | tr -d ' \n'
    else
        die "openssl or od is required to generate NYRO_ADMIN_TOKEN"
    fi
}

# Make sure RUNTIME_IMAGE is in the local image store for TARGET_PLATFORM, so
# `docker build` does not depend on a registry round-trip. A mirrored copy is
# retagged as RUNTIME_IMAGE, keeping the Dockerfile reference unchanged.
ensure_runtime_image() {
    local first mirror path mirrored

    if [[ "$(docker image inspect --format '{{.Os}}/{{.Architecture}}' "${RUNTIME_IMAGE}" 2>/dev/null)" == "${TARGET_PLATFORM}" ]]; then
        return
    fi
    echo "Pulling base image ${RUNTIME_IMAGE}"
    if docker pull --platform "${TARGET_PLATFORM}" "${RUNTIME_IMAGE}"; then
        return
    fi

    # Mirrors only stand in for Docker Hub references (no registry host).
    first="${RUNTIME_IMAGE%%/*}"
    if [[ "${RUNTIME_IMAGE}" == */* && ( "${first}" == *.* || "${first}" == *:* || "${first}" == localhost ) ]]; then
        die "failed to pull ${RUNTIME_IMAGE}"
    fi
    path="${RUNTIME_IMAGE}"
    [[ "${path}" == */* ]] || path="library/${path}"

    for mirror in ${DOCKER_MIRRORS}; do
        mirrored="${mirror}/${path}"
        echo "Docker Hub pull failed; trying mirror ${mirrored}"
        if docker pull --platform "${TARGET_PLATFORM}" "${mirrored}"; then
            docker tag "${mirrored}" "${RUNTIME_IMAGE}"
            docker rmi "${mirrored}" >/dev/null
            return
        fi
    done
    die "failed to pull ${RUNTIME_IMAGE} from Docker Hub and mirrors (${DOCKER_MIRRORS}); set NYRO_DOCKER_MIRRORS or RUNTIME_IMAGE"
}

# A dynamically linked binary built on a newer distro can require a glibc the
# runtime image lacks; the container would then crash-loop on start. Static
# binaries skip this check.
ensure_glibc_compatible() {
    local required available

    [[ "${BINARY_DESCRIPTION}" == *"dynamically linked"* ]] || return 0
    command -v objdump >/dev/null 2>&1 \
        || die "objdump (binutils) is required to check glibc compatibility; or run 'make server-musl' for a static binary"
    required="$(objdump -T "${SERVER_BINARY}" | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' | sed 's/^GLIBC_//' | sort -Vu | tail -1)"
    [[ -n "${required}" ]] || return 0
    available="$(docker run --rm --network none --platform "${TARGET_PLATFORM}" --entrypoint ldd \
        "${RUNTIME_IMAGE}" --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+$' || true)"
    [[ -n "${available}" ]] || die "could not determine the glibc version of ${RUNTIME_IMAGE}"
    if [[ "$(printf '%s\n%s\n' "${required}" "${available}" | sort -V | tail -1)" != "${available}" ]]; then
        die "${SERVER_BINARY} needs glibc ${required} but ${RUNTIME_IMAGE} has ${available}; the container would fail to start. Run 'make server-musl' (static binary) and retry"
    fi
    echo "glibc check: binary needs ${required}, ${RUNTIME_IMAGE} has ${available}"
}

ensure_compose_env() {
    local admin_token proxy_auth_key

    if [[ -e "${ENV_FILE}" && ! -f "${ENV_FILE}" ]]; then
        die "${ENV_FILE} exists but is not a regular file"
    fi

    # nyro runs with host networking and binds 0.0.0.0 directly on the host,
    # so the data plane must always carry a Bearer key; generate one when missing.
    if [[ -f "${ENV_FILE}" ]]; then
        if ! grep -Eq '^NYRO_PROXY_AUTH_KEY=.+$' "${ENV_FILE}"; then
            if grep -Eq '^NYRO_PROXY_AUTH_KEY=$' "${ENV_FILE}"; then
                die "NYRO_PROXY_AUTH_KEY is empty in ${ENV_FILE}"
            fi
            proxy_auth_key="$(generate_admin_token)"
            printf '\nNYRO_PROXY_AUTH_KEY=%s\n' "${proxy_auth_key}" >> "${ENV_FILE}"
            chmod 0600 "${ENV_FILE}"
            echo "Added NYRO_PROXY_AUTH_KEY to ${ENV_FILE}"
        fi

        if grep -Eq '^NYRO_ADMIN_TOKEN=.+$' "${ENV_FILE}"; then
            chmod 0600 "${ENV_FILE}"
            return
        fi
        if grep -Eq '^NYRO_ADMIN_TOKEN=$' "${ENV_FILE}"; then
            die "NYRO_ADMIN_TOKEN is empty in ${ENV_FILE}"
        fi

        admin_token="$(generate_admin_token)"
        printf '\nNYRO_ADMIN_TOKEN=%s\n' "${admin_token}" >> "${ENV_FILE}"
        chmod 0600 "${ENV_FILE}"
        echo "Added NYRO_ADMIN_TOKEN to ${ENV_FILE}"
        return
    fi

    admin_token="$(generate_admin_token)"
    proxy_auth_key="$(generate_admin_token)"
    (
        umask 077
        printf '# Generated by make-docker.sh; keep this file private.\n' > "${ENV_FILE}"
        printf 'NYRO_ADMIN_TOKEN=%s\n' "${admin_token}" >> "${ENV_FILE}"
        printf 'NYRO_PROXY_AUTH_KEY=%s\n' "${proxy_auth_key}" >> "${ENV_FILE}"
    )
    echo "Created ${ENV_FILE} with random NYRO_ADMIN_TOKEN and NYRO_PROXY_AUTH_KEY"
}

command -v docker >/dev/null 2>&1 || die "docker is required"
command -v file >/dev/null 2>&1 || die "file is required"
docker info >/dev/null 2>&1 || die "the Docker daemon is not available"

[[ -x "${SERVER_BINARY}" ]] || die "${SERVER_BINARY} is missing; run 'make server-musl' first"

BINARY_DESCRIPTION="$(file -b "${SERVER_BINARY}")"
case "${BINARY_DESCRIPTION}" in
    *ELF*x86-64*) TARGET_PLATFORM="linux/amd64" ;;
    *ELF*ARM*aarch64*) TARGET_PLATFORM="linux/arm64" ;;
    *)
        die "unsupported server binary (${BINARY_DESCRIPTION}); build it on Linux amd64 or arm64"
        ;;
esac

NYRO_VERSION="$("${SERVER_BINARY}" --version | awk 'NF >= 2 { print $2; exit }')"
[[ -n "${NYRO_VERSION}" ]] || NYRO_VERSION="dev"
echo "Server binary: ${SERVER_BINARY} (built $(date -r "${SERVER_BINARY}" '+%Y-%m-%d %H:%M:%S'))"

ensure_runtime_image
ensure_glibc_compatible

BUILD_CONTEXT="$(mktemp -d)"
trap cleanup EXIT
install -m 0755 "${SERVER_BINARY}" "${BUILD_CONTEXT}/nyro-server"

echo "Building ${IMAGE_TAG} for ${TARGET_PLATFORM} from Nyro ${NYRO_VERSION}"
docker build \
    --platform "${TARGET_PLATFORM}" \
    --build-arg "NYRO_VERSION=${NYRO_VERSION}" \
    --build-arg "RUNTIME_IMAGE=${RUNTIME_IMAGE}" \
    --file "${ROOT_DIR}/Dockerfile" \
    --tag "${IMAGE_TAG}" \
    "${BUILD_CONTEXT}"

TEMP_ARCHIVE="$(mktemp "${ROOT_DIR}/.nyro-mine.tar.XXXXXX")"
echo "Exporting ${IMAGE_TAG} to ${OUTPUT_ARCHIVE}"
docker image save --output "${TEMP_ARCHIVE}" "${IMAGE_TAG}"
chmod 0644 "${TEMP_ARCHIVE}"
mv -f -- "${TEMP_ARCHIVE}" "${OUTPUT_ARCHIVE}"
TEMP_ARCHIVE=""

ensure_compose_env

echo "Done:"
echo "  image:   ${IMAGE_TAG}"
echo "  archive: ${OUTPUT_ARCHIVE}"
echo "  next:    docker compose up -d"
