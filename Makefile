# Thin wrapper over the commands in CLAUDE.md. Recipes avoid POSIX-only syntax
# so they work whether make picks sh or cmd.exe. The Linux targets build in a
# Docker container, so they need only Docker (Linux binaries in target-linux/release).

PS      := powershell -NoProfile -ExecutionPolicy Bypass
FILE    ?=
CLI_ARGS ?=

.PHONY: all linux linux-test linux-clean build test clippy fmt run cli dry-run installer installer-test clean help

all: build

help:
	@echo make build              - release build, GUI and CLI exes in target/release
	@echo make test               - cargo test, unit tests
	@echo make clippy             - cargo clippy
	@echo make fmt                - cargo fmt
	@echo make run FILE=x.mp3     - run the GUI, FILE is optional
	@echo make dry-run FILE=x.mp3 - run the CLI with --dry-run
	@echo make cli FILE=x.mp3 CLI_ARGS=...  - run the CLI
	@echo make installer          - build the NSIS installer into dist/, SKIP_CARGO=1 reuses the exes
	@echo make installer-test     - build the -test installer, forces downloads
	@echo make linux              - Linux release build in Docker, binaries in target-linux/release
	@echo make linux-test         - cargo test inside the Linux container
	@echo make linux-clean        - remove target-linux/ and the cargo cache volume
	@echo make clean              - cargo clean and remove dist/

build:
	cargo build --release

# Linux build in a container: a separate target dir (the Windows one is not
# reusable) and a named volume for the cargo registry so reruns stay fast.
# Git Bash would rewrite /src in the docker arguments into a Windows path.
export MSYS_NO_PATHCONV := 1
LINUX_IMAGE ?= rust:1-bookworm
LINUX_DEPS  := libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev libgl1-mesa-dev pkg-config
DOCKER_RUN  := docker run --rm -v "$(CURDIR):/src" -v radio-track-splitter-cargo:/usr/local/cargo/registry -w /src -e CARGO_TARGET_DIR=/src/target-linux $(LINUX_IMAGE)

linux:
	$(DOCKER_RUN) sh -c "apt-get update -qq && apt-get install -y -qq --no-install-recommends $(LINUX_DEPS) >/dev/null && cargo build --release"

linux-test:
	$(DOCKER_RUN) sh -c "apt-get update -qq && apt-get install -y -qq --no-install-recommends $(LINUX_DEPS) >/dev/null && cargo test --release"

linux-clean:
	$(PS) -Command "Remove-Item -Recurse -Force -ErrorAction SilentlyContinue target-linux"
	docker volume rm radio-track-splitter-cargo

test:
	cargo test

clippy:
	cargo clippy --all-targets

fmt:
	cargo fmt

run:
	cargo run --release --bin radio-track-splitter -- $(FILE)

cli:
	cargo run --release --bin radio-track-splitter-cli -- $(FILE) $(CLI_ARGS)

dry-run:
	cargo run --release --bin radio-track-splitter-cli -- $(FILE) --dry-run $(CLI_ARGS)

# build.ps1 runs cargo build itself; reuse the existing exes with SKIP_CARGO=1.
installer:
	$(PS) -File installer\build.ps1 $(if $(SKIP_CARGO),-SkipCargo)

installer-test:
	$(PS) -File installer\build.ps1 -ForceDownloads $(if $(SKIP_CARGO),-SkipCargo)

clean:
	cargo clean
	$(PS) -Command "Remove-Item -Recurse -Force -ErrorAction SilentlyContinue dist"
