# Every target runs inside Docker — no local Rust or Stellar toolchain needed.
#
#   make test    unit tests (native compilation, soroban-sdk testutils)
#   make build   release .wasm for wasm32v1-none
#   make fmt     format
#   make lint    clippy, warnings denied
#
# The cargo registry and the target directory live in named volumes, so
# rebuilds are incremental and no root-owned files land in the working tree.

IMAGE  := trakx/soroban-build
TARGET := wasm32v1-none

DOCKER_RUN = docker run --rm -t \
	-v "$(CURDIR)":/work \
	-v soroban-cargo-registry:/usr/local/cargo/registry \
	-v soroban-target:/work/target \
	-w /work $(IMAGE)

.PHONY: image test build fmt fmt-check lint check clean

image:
	@docker build -q -f docker/Dockerfile -t $(IMAGE) . >/dev/null

test: image
	$(DOCKER_RUN) cargo test

build: image
	$(DOCKER_RUN) cargo build --target $(TARGET) --release
	@$(DOCKER_RUN) sh -c 'mkdir -p /work/artifacts && cp target/$(TARGET)/release/*.wasm /work/artifacts/ && ls -l /work/artifacts'

fmt: image
	$(DOCKER_RUN) cargo fmt --all

fmt-check: image
	$(DOCKER_RUN) cargo fmt --all -- --check

lint: image
	$(DOCKER_RUN) cargo clippy --all-targets -- -D warnings

check: fmt-check lint test

clean:
	docker volume rm -f soroban-target soroban-cargo-registry
	rm -rf artifacts
