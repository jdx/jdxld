ARG BASE_IMAGE=ghcr.io/wild-linker/wild/ci-ubuntu-amd64:latest@sha256:85e2546a85c2b791cd9169f30a18e33d78d58ab68dc07307a650966f63a35cfc
FROM ${BASE_IMAGE}

RUN eatmydata rustup toolchain install nightly \
        --allow-downgrade \
        --target $(uname -m)-unknown-linux-musl \
        --component rustc-codegen-cranelift-preview
