# Build a native server for CentOS 7.6

[中文](<centos7-build_CN.md>) · [Server configuration](<README.md>)

This recipe builds the current source into a native Linux x86_64 server **with the WebUI embedded**, using CentOS 7.6 userland. Docker is a build dependency, not a runtime requirement on the target VPS. The builder image is **not a production service image**: the commands below neither install a service nor start Nyro on the VPS.

> CentOS 7 is end-of-life. Matching its ABI does not restore security maintenance. Prefer migrating the server to a supported OS; treat this recipe as a compatibility bridge. The recipe and its checks are not a claim that a particular build has already passed on your VPS.

This recipe has been exercised end to end once: it produced a `nyro-server` that started on a CentOS 7.6.1810 host (kernel `3.10.0-957`, glibc `2.17-260`) and answered the loopback health/readiness probes with the embedded WebUI served. That is evidence for this source revision and toolchain set, not a guarantee for future changes or other hosts; re-run the checks below for your own build.

## Prerequisites and build

Use a Docker host with Buildx and permission to access the Docker daemon. A **local Linux AMD64 builder is recommended**. The script always requests `linux/amd64`; ARM hosts need working AMD64 emulation and can be substantially slower. Docker access is privileged even when the command is run without `sudo`.

No Rust or Node installation is required on the host or VPS. The initial build needs network access to container registries, CentOS Vault, Rust distributions, crates.io, and the npm registry, plus sufficient disk space and RAM. Lower the job count if the compiler is killed for lack of memory.

Run from the repository root:

```bash
./scripts/build-centos7.sh
# Equivalent: make server-centos7

# Limit Rust compilation parallelism and choose the export directory.
./scripts/build-centos7.sh --jobs 2 --output "$PWD/target/centos7"

# Prepare only the reusable toolchain image; do not compile Nyro or its WebUI.
./scripts/build-centos7.sh --image-only

# Reuse an existing Cargo registry; prohibit Cargo network access for this build.
./scripts/build-centos7.sh --cargo-registry "$HOME/.cargo/registry" --offline
```

The [build script](<../../scripts/build-centos7.sh>) accepts:

| Option | Default | Meaning |
| --- | --- | --- |
| `--jobs N` | `BUILD_JOBS`, or `4` | Positive Rust compilation job count; does not limit frontend build concurrency. |
| `--output DIR` | `target/centos7` under the repository | Export directory (commas are rejected because Buildx uses CSV syntax). A relative custom path is relative to the invoking shell's working directory. |
| `--image-only` | Off | Build and load the reusable toolchain image without exporting a server binary. |
| `--image NAME` | `nyro-builder:centos7.6-rust1.96.0` | Tag for the loaded builder image; does not select a different base image/toolchain. |
| `--cargo-registry DIR` | None | Seed the container Cargo registry from a trusted local registry directory containing `index/` and `cache/`. Pass the registry directory, not all of `CARGO_HOME`. Relative paths resolve from the invoking working directory; spaces are supported. |
| `--offline` | Off | Set `CARGO_NET_OFFLINE=true` for Cargo compilation only; does not make Docker, toolchain, or frontend steps offline. |
| `--help` | — | Show usage. |

The default flow first loads the builder image, then builds the artifact stage. Subsequent invocations can reuse BuildKit layers and Cargo caches on the same builder. An image-only build does not necessarily populate the Cargo dependency cache needed for an offline source build.

### Cached Cargo rebuilds

`--cargo-registry` is optional; the default build permits Cargo downloads. When supplied, the directory is passed as the `cargo-seed` Buildx context and copied into the container's registry cache without overwriting existing entries. Use a trusted Cargo registry containing the needed index entries and crate archives (and, optionally, extracted sources). Do not put credentials or unrelated private files in this directory: its contents are made available to the selected Docker builder. The standard registry directory does not include the parent Cargo credential/configuration files.

This reuses **dependency source/index data, not host compiled targets or shared libraries**. Rust and native C dependencies are still compiled and linked in the CentOS userland; existing target cache reuse is confined to the builder's own Cargo target cache. You can also use `--offline` without a registry argument once that builder's caches are populated. An incomplete cache causes Cargo to fail rather than silently access the network.

`--offline` applies **only to Cargo**. Container images, Dockerfile frontend, Vault RPMs, Rust archives, pnpm, and frontend dependencies still require network access unless their corresponding build layers/downloads are already cached. `--image-only` does not consume the registry seed or run Cargo, so it does not establish Cargo offline readiness. These cache options do not attest to successful execution on the target VPS.

After a successful full build, the chosen output directory contains:

| Output | Contents |
| --- | --- |
| `nyro-server` | Stripped native GNU/Linux x86_64 release executable, including WebUI assets. |
| `SHA256SUMS` | SHA-256 checksum of the executable. |
| `build-info.txt` | Builder RPM/compiler versions and executable ELF dynamic information. |

These are build artifacts, not a configured deployment. A failed build can leave older files in a reused output directory; check the build exit status and use a fresh directory when distinguishing releases.

## What is pinned and checked

The [CentOS build Dockerfile](<../../deploy/centos7/Dockerfile>) is the source of truth for image digests, Rust archive checksum, package checks, and build flags:

- CentOS `7.6.1810` base image pinned by digest; only the `7.6.1810/os/x86_64` Vault repository is enabled. There is no `yum update` to newer CentOS userland.
- Package assertions for `centos-release-7-6.1810.2`, glibc `2.17-260`, and libgcc/libstdc++ `4.8.5-36` (full RPM identifiers are in the Dockerfile). Native compilation uses the CentOS GCC toolchain and development headers.
- Official Rust/Cargo `1.96.0`, installed from individually SHA-256-verified `rustc`, `cargo`, and `rust-std` archives for `x86_64-unknown-linux-gnu` (unused documentation and optional tools are not downloaded). Edition 2024 alone does not imply Rust 1.85 is sufficient: the locked server graph includes `home 0.5.12`, requiring Rust 1.88, and source uses let-chains.
- Generic `x86-64` code generation, GCC as linker driver, and `-C linker-features=-lld` to use the CentOS linker. Do not replace these with `target-cpu=native` for a portable VPS artifact.
- Frontend build in a separate digest-pinned Node `22.22.0` image using pnpm `11.0.0` and a frozen frontend lockfile. Only generated WebUI assets enter the CentOS source build; the modern frontend userland is not used to link the server.
- `cargo build --locked -p nyro-server --release --target x86_64-unknown-linux-gnu`. The default embedded-WebUI feature is enabled; desktop/Tauri binaries are not built.

The selected native dependencies include ring, bundled SQLite, and Zstd. The [binary verification script](<../../deploy/centos7/verify-binary.sh>) checks x86_64 architecture, a runtime dependency allowlist, required GLIBC symbol versions no newer than 2.17, and dynamic loading/`--version` within the builder. Check the emitted build output rather than assuming these gates passed.

### Build-time certificates are not a runtime CA bundle

A modern certificate bundle from the Node base image is copied into the builder and selected for build downloads. It does **not** replace CentOS shared libraries, is **not embedded as the server's runtime trust store**, and is **not exported** with the three artifacts. Runtime upstream TLS still needs a valid, appropriately maintained CA trust configuration on the VPS. Do not disable TLS verification to work around expired roots.

### Userland is not the kernel

Docker shares the build host's kernel. The container reproduces old userspace libraries, **not Linux `3.10.0-957`**. ELF checks and container execution cannot prove that every runtime syscall or networking path works on that VPS. Validate the native executable on the actual target before deploying it. This also does not make ordinary Ubuntu-built release binaries compatible with CentOS 7.

## Copy and smoke-test without installing a service

The following generic example transfers the native artifacts to a **new, dedicated directory** on a target reachable over SSH. Replace `user@example-vps` and the example directory; do not overwrite a running production binary.

```bash
# On the build host, after a successful build:
(cd target/centos7 && sha256sum -c SHA256SUMS)
ssh user@example-vps 'mkdir -m 700 "$HOME/nyro-centos7-check"'
scp target/centos7/{nyro-server,SHA256SUMS,build-info.txt} \
  user@example-vps:nyro-centos7-check/
ssh user@example-vps

# Now on the VPS:
cd "$HOME/nyro-centos7-check"
sha256sum -c SHA256SUMS
chmod 700 nyro-server
./nyro-server --version
./nyro-server --help
```

`--version` and `--help` exit before gateway initialization. For an automated check, copy [smoke.sh](<../../deploy/centos7/smoke.sh>) to the VPS and run `bash smoke.sh /absolute/path/to/nyro-server`. It checks that ports 29530/29531 are unused, creates isolated scratch data and HOME, probes health/readiness and embedded WebUI, and stops the temporary server (90-second timeout, followed by up to 5 seconds of termination grace). Scratch data and logs are retained at the printed path for inspection.

For a manual HTTP smoke test, ensure the chosen ports are unused and start in the foreground with **fresh scratch storage**, not production data. This example uses a clean environment to avoid inheriting `NYRO_*` configuration or database DSNs:

```bash
scratch=$(mktemp -d /tmp/nyro-centos7-smoke.XXXXXX)
env -i HOME="$HOME" PATH=/usr/bin:/bin ./nyro-server \
  --mode all --storage-backend sqlite --data-dir "$scratch" \
  --proxy-host 127.0.0.1 --proxy-port 29530 \
  --admin-host 127.0.0.1 --admin-port 29531
```

From a second SSH session on the VPS:

```bash
curl --fail --silent --show-error --max-time 5 http://127.0.0.1:29530/health
curl --fail --silent --show-error --max-time 5 http://127.0.0.1:29530/readyz
curl --fail --silent --show-error --max-time 5 http://127.0.0.1:29531/healthz
curl --fail --silent --show-error --max-time 5 http://127.0.0.1:29531/readyz
```

Healthy responses are HTTP 200 with `{"status":"ok"}`. Readiness can return HTTP 503 with `schema_pending` or `unavailable`. These unauthenticated probes do not invoke model inference, but **server startup is not read-only**: it initializes scratch storage and can download/write a model-catalog cache; configured providers would also enable background usage/OAuth activity. The clean environment may omit a custom CA configuration; configure trust explicitly if needed.

Stop the foreground process with Ctrl-C. Retain logs and the scratch path for inspection, then remove only confirmed disposable files when no longer needed. No firewall opening, systemd unit, production credentials, or Rust installation on the VPS is required for this check. Production deployment, authentication, persistence, runtime CA maintenance, and service supervision are separate steps; see [Server configuration](<README.md>).
