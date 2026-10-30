ARG BASE_IMAGE=ghcr.io/wild-linker/wild/ci-ubuntu-base-amd64:latest@sha256:9ef64a4a344f1192edf808016cc78022662845d081a6bcc9902fe3d5493ab488
FROM ${BASE_IMAGE}

RUN eatmydata apt-get update \
    && eatmydata apt-get -y install \
        gcc-multilib \
        wabt \
        wasi-libc \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

RUN eatmydata rustup target add wasm32-wasip1
