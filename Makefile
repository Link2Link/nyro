.PHONY: dev build server server-musl server-gnu server-slim server-centos7 tools check test test-core test-server fmt fmt-check clean webui smoke smoke-storage release-check help

# Development — start Tauri desktop app with hot reload
dev: webui-build
	cargo tauri build --config src-tauri/tauri.dev.conf.json

# Build desktop app (release)
build: webui-build
	cargo tauri build

# Static musl target for this Linux host (empty on other systems).
HOST_OS := $(shell uname -s)
HOST_ARCH := $(shell uname -m | sed -e 's/^arm64$$/aarch64/')
MUSL_TARGET := $(if $(filter Linux,$(HOST_OS)),$(HOST_ARCH)-unknown-linux-musl)

# Build server binary (release, webui embedded). On Linux it is statically linked
# against musl and does not depend on the system C library, so it runs on any
# Linux (Debian images, CentOS 7, Alpine, iStoreOS); output:
# target/<arch>-unknown-linux-musl/release/nyro-server. Needs `rustup target add
# <arch>-unknown-linux-musl` and musl-tools (musl-gcc). Other hosts build natively.
server: webui-build
ifneq ($(MUSL_TARGET),)
	cargo build -p nyro-server --release --target $(MUSL_TARGET)
else
	@echo "musl static build is Linux-only; building for $(HOST_OS) natively"
	cargo build -p nyro-server --release
endif

# Alias kept for existing scripts and habits.
server-musl: server

# Build server binary dynamically linked against the system glibc (release,
# webui embedded); output: target/release/nyro-server. Runs only on hosts whose
# glibc is at least as new as the build machine's.
server-gnu: webui-build
	cargo build -p nyro-server --release

# Build slim server binary (release, no embedded webui, no pnpm build required)
server-slim:
	cargo build -p nyro-server --release --no-default-features

# Build a CentOS 7.6/glibc 2.17 compatible server in Docker (with embedded WebUI)
server-centos7:
	bash scripts/build-centos7.sh

# Build nyro-tools binary (release)
tools:
	cargo build -p nyro-tools --release

# Run server binary locally (debug, webui embedded)
server-dev: webui-build
	cargo run -p nyro-server -- --proxy-port 19530 --admin-port 19531

# Build webui
webui-build:
	cd webui && pnpm install && pnpm build

# Format all Rust code
fmt:
	cargo fmt --all

# Check Rust formatting (non-destructive, used in CI)
fmt-check:
	cargo fmt --all -- --check

# Type check & lint everything
check: fmt-check
	cargo check --workspace
	cd webui && pnpm build

# Run all Rust tests across the workspace
test:
	cargo test --workspace --exclude nyro-desktop --no-default-features

# Run tests for nyro-core only
test-core:
	cargo test -p nyro-core

# Run tests for nyro-server only
test-server:
	cargo test -p nyro-server --no-default-features

# End-to-end smoke (local mock upstream + nyro-server)
smoke:
	python3 scripts/smoke/server_smoke.py

# Storage backend smoke (default: sqlite + postgres)
smoke-storage:
	python3 scripts/smoke/storage_backends_smoke.py

# Pre-release verification gate
release-check: check smoke

# Clean all build artifacts
clean:
	cargo clean
	rm -rf webui/dist webui/node_modules/.vite

help:
	@echo "Nyro AI Gateway"
	@echo ""
	@echo "  make dev          Start Tauri desktop app (dev mode)"
	@echo "  make build        Build desktop app (release)"
	@echo "  make server       Build static musl server binary (release, webui embedded)"
	@echo "  make server-gnu   Build server binary linked against system glibc"
	@echo "  make server-slim  Build slim server binary (release, no embedded webui)"
	@echo "  make server-centos7 Build CentOS 7.6 compatible server in Docker"
	@echo "  make tools        Build nyro-tools binary (release)"
	@echo "  make server-dev   Run server binary (debug)"
	@echo "  make webui-build  Build frontend only"
	@echo "  make fmt          Format Rust code"
	@echo "  make fmt-check    Check Rust formatting (CI)"
	@echo "  make check        Type check Rust + TypeScript"
	@echo "  make test         Run all Rust tests (workspace)"
	@echo "  make test-core    Run nyro-core tests only"
	@echo "  make test-server  Run nyro-server tests only"
	@echo "  make smoke        Run local server smoke tests"
	@echo "  make smoke-storage Run storage smoke tests (default sqlite + postgres)"
	@echo "  make release-check Run check + smoke before release"
	@echo "  make clean        Remove build artifacts"
