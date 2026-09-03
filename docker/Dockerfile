# Build and test environment for the Soroban contracts.
#
# Nothing in this repository requires a local Rust or Stellar toolchain: the
# image below is the toolchain. See the Makefile for the entry points.
FROM rust:1.91-slim

# Resolve the pinned toolchain at image build time so that cargo does not
# re-sync it on every container start. `rustup show` alone does not reliably
# materialise the targets and components declared in rust-toolchain.toml, so
# they are installed explicitly.
COPY rust-toolchain.toml /tmp/rust-toolchain.toml
RUN cd /tmp \
    && rustup show \
    && rustup target add wasm32v1-none \
    && rustup component add rustfmt clippy \
    && rm rust-toolchain.toml

WORKDIR /work
