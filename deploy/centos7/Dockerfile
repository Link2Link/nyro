# No `# syntax=` frontend pin: the bundled BuildKit frontend supports every
# instruction used here, and pinning forces a Docker Hub tag resolution on
# every build (a common failure when registry auth is flaky).
# Match the VPS userland, not the Docker host kernel. Never yum update here.
ARG CENTOS_IMAGE=centos:7.6.1810@sha256:747b2de199b6197a26eb1a24d69740d25483995842b2d2f75824095e9d1d19eb
ARG NODE_IMAGE=node:22.22.0-bookworm-slim@sha256:7cc56ef285a8568121537d17b05e72128f01b89c54607b51acf084a50ef483f3
FROM ${NODE_IMAGE} AS node-base
# node:slim has no system CA bundle; export Node's bundled Mozilla roots.
RUN node -e "require('fs').writeFileSync('/opt/build-ca.pem', require('tls').rootCertificates.join('\\n') + '\\n')"

FROM node-base AS webui
ARG PNPM_VERSION=11.0.0
RUN npm install --global pnpm@${PNPM_VERSION}
WORKDIR /webui
COPY webui/package.json webui/pnpm-lock.yaml webui/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY webui/ ./
RUN pnpm build

FROM ${CENTOS_IMAGE} AS builder
ARG RUST_VERSION=1.96.0
ARG RUSTC_SHA256=7d7fa1d0cfb0fab71a956bb78f41107202c17f30ab56c45288e869a37fd9633d
ARG CARGO_SHA256=dee75c3c8f9f600ad75bc0c93249e767d3047845a4dd668327ce43ab039ba266
ARG RUST_STD_SHA256=c09c7c646248f14f473f5f7a029af15ee57c3a9f9bc93dfa72d9621938586b82
# Modern trust roots are build-only; do not replace the target system libraries.
COPY --from=node-base /opt/build-ca.pem /opt/build-ca.pem
ENV CURL_CA_BUNDLE=/opt/build-ca.pem \
    SSL_CERT_FILE=/opt/build-ca.pem \
    CARGO_HTTP_CAINFO=/opt/build-ca.pem
RUN printf '[vault-os]\nname=CentOS 7.6.1810 OS\nbaseurl=https://vault.centos.org/7.6.1810/os/x86_64/\nenabled=1\ngpgcheck=1\ngpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-CentOS-7\nsslcacert=/opt/build-ca.pem\n' > /etc/yum.repos.d/nyro-vault.repo \
    && yum --disablerepo='*' --enablerepo=vault-os -y install \
        gcc gcc-c++ make binutils glibc-devel libstdc++-devel pkgconfig perl curl tar xz gzip \
    && yum clean all \
    && test "$(rpm -q centos-release)" = centos-release-7-6.1810.2.el7.centos.x86_64 \
    && test "$(rpm -q glibc)" = glibc-2.17-260.el7.x86_64 \
    && test "$(rpm -q libgcc)" = libgcc-4.8.5-36.el7.x86_64 \
    && test "$(rpm -q libstdc++)" = libstdc++-4.8.5-36.el7.x86_64
# Download only the three required components (no docs/clippy/rustfmt).
RUN set -eu; pids=''; \
    for component in rustc cargo rust-std; do \
        curl --fail --silent --show-error --location --retry 5 --connect-timeout 30 \
            "https://static.rust-lang.org/dist/${component}-${RUST_VERSION}-x86_64-unknown-linux-gnu.tar.xz" \
            -o "/tmp/${component}.tar.xz" & pids="$pids $!"; \
    done; \
    for pid in $pids; do wait "$pid"; done; \
    printf '%s  /tmp/rustc.tar.xz\n%s  /tmp/cargo.tar.xz\n%s  /tmp/rust-std.tar.xz\n' \
        "$RUSTC_SHA256" "$CARGO_SHA256" "$RUST_STD_SHA256" | sha256sum -c -; \
    for component in rustc cargo rust-std; do \
        mkdir "/tmp/$component"; \
        tar -xJf "/tmp/$component.tar.xz" -C "/tmp/$component" --strip-components=1; \
        "/tmp/$component/install.sh" --prefix=/opt/rust --disable-ldconfig; \
    done; \
    rm -rf /tmp/rustc /tmp/cargo /tmp/rust-std /tmp/rustc.tar.xz /tmp/cargo.tar.xz /tmp/rust-std.tar.xz
ENV PATH=/opt/rust/bin:$PATH \
    CARGO_HOME=/opt/cargo \
    CARGO_TARGET_DIR=/opt/target \
    CARGO_INCREMENTAL=0 \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=gcc \
    RUSTFLAGS="-C target-cpu=x86-64 -C linker=gcc -C linker-features=-lld"
WORKDIR /src
RUN rustc --version && cargo --version && gcc --version

# Override this named context with a host Cargo registry directory to seed sources.
FROM scratch AS cargo-seed
COPY deploy/centos7/seed-placeholder/ /

FROM builder AS build
ARG BUILD_JOBS=4
ARG CARGO_OFFLINE=false
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY src-server/ src-server/
COPY src-tauri/ src-tauri/
COPY --from=webui /webui/dist/ webui/dist/
COPY deploy/centos7/verify-binary.sh /usr/local/bin/verify-binary
RUN --mount=type=cache,id=nyro-centos76-cargo,target=/opt/cargo,sharing=locked \
    --mount=type=cache,id=nyro-centos76-target,target=/opt/target,sharing=locked \
    --mount=type=bind,from=cargo-seed,target=/opt/cargo-seed \
    mkdir -p /opt/cargo/registry \
    && cp -an /opt/cargo-seed/. /opt/cargo/registry/ \
    && CARGO_NET_OFFLINE="${CARGO_OFFLINE}" CARGO_HTTP_TIMEOUT=120 CARGO_NET_RETRY=5 \
       cargo build --locked -p nyro-server --release --target x86_64-unknown-linux-gnu --jobs "${BUILD_JOBS}" \
    && mkdir /out \
    && cp /opt/target/x86_64-unknown-linux-gnu/release/nyro-server /out/nyro-server \
    && strip /out/nyro-server \
    && bash /usr/local/bin/verify-binary /out/nyro-server \
    && /out/nyro-server --version \
    && { rpm -q centos-release glibc libgcc libstdc++ glibc-devel kernel-headers; rustc --version; cargo --version; gcc --version; sha256sum Cargo.lock; objdump -p /out/nyro-server; } > /out/build-info.txt \
    && cd /out && sha256sum nyro-server > SHA256SUMS

FROM scratch AS artifacts
COPY --from=build /out/ /
