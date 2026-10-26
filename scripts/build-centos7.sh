#!/usr/bin/env bash
# Build the native GNU/Linux server for CentOS 7.6 (glibc 2.17), including WebUI.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
output="$root/target/centos7"
image=nyro-builder:centos7.6-rust1.96.0
jobs=${BUILD_JOBS:-4}
image_only=false
cargo_registry=
offline=false
usage() {
    printf '%s\n' \
        'Usage: scripts/build-centos7.sh [--output DIR] [--jobs N] [--image-only] [--image NAME]' \
        '' \
        'Default: build the reusable builder image, then export nyro-server (with WebUI),' \
        'SHA256SUMS and build-info.txt to target/centos7. Requires Docker + Buildx.' \
        'All compilation runs in containers; no host Rust, Node or root required.' \
        '--image-only builds/tags the toolchain image without compiling Nyro.' \
        '--cargo-registry DIR seeds Cargo registry sources/index (not host binaries or credentials).' \
        '--offline disables Cargo network access; base images/toolchains/frontend still need caches.'
}
while (($#)); do
    case "$1" in
        --output|--jobs|--image|--cargo-registry)
            (($# >= 2)) || { usage >&2; exit 2; }
            case "$1" in
                --output) output=$2 ;;
                --jobs) jobs=$2 ;;
                --image) image=$2 ;;
                --cargo-registry) cargo_registry=$2 ;;
            esac
            shift 2 ;;
        --image-only) image_only=true; shift ;;
        --offline) offline=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done
[[ $jobs =~ ^[1-9][0-9]*$ ]] || { echo '--jobs must be a positive integer' >&2; exit 2; }
[[ -n $output && $output != *','* ]] || { echo '--output must be nonempty and cannot contain commas (Buildx CSV syntax)' >&2; exit 2; }
[[ -n $image ]] || { echo '--image must be nonempty' >&2; exit 2; }
artifact_args=(--build-arg "CARGO_OFFLINE=$offline")
if [[ -n $cargo_registry ]]; then
    [[ -d $cargo_registry/index && -d $cargo_registry/cache ]] || {
        echo '--cargo-registry must contain index/ and cache/ (pass registry/, not CARGO_HOME)' >&2; exit 2;
    }
    cargo_registry=$(cd -- "$cargo_registry" && pwd)
    artifact_args+=(--build-context "cargo-seed=$cargo_registry")
fi
command -v docker >/dev/null || { echo 'Docker is required' >&2; exit 1; }
docker info >/dev/null
docker buildx version >/dev/null
common=(--platform linux/amd64 --progress plain --file "$root/deploy/centos7/Dockerfile")
docker buildx build "${common[@]}" --target builder --tag "$image" --load "$root"
if [[ $image_only == true ]]; then
    printf 'Builder image: %s\n' "$image"
    exit 0
fi
mkdir -p -- "$output"
output=$(cd -- "$output" && pwd)
docker buildx build "${common[@]}" "${artifact_args[@]}" --target artifacts --build-arg "BUILD_JOBS=$jobs" \
    --output "type=local,dest=$output" "$root"
printf '\nCentOS 7.6 artifacts: %s\nBuilder image: %s\n' "$output" "$image"
