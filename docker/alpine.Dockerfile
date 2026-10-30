# Run on a recent version of alpine Linux
#
# docker build --progress=plain -t jdxld-dev-alpine . -f docker/alpine.Dockerfile
# docker run -it jdxld-dev-alpine

FROM rust:1.99.0-alpine@sha256:0cce0a5e0e8ba67b455257a3a02a1d99005f382748789d6464460028810f1627 AS chef
RUN wget -qO- https://github.com/LukeMathWalker/cargo-chef/releases/download/v0.1.70/cargo-chef-x86_64-unknown-linux-musl.tar.gz | tar -xzf- && \
    mv cargo-chef /usr/local/bin
RUN rustup toolchain install nightly && \
    rustup component add rustc-codegen-cranelift-preview --toolchain nightly

RUN apk add build-base lld clang clang-extra-tools bash

WORKDIR /jdxld

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /jdxld/recipe.json recipe.json
RUN cargo chef cook --all-targets --recipe-path recipe.json
COPY . .
