FROM rust:1.99.0-alpine@sha256:0cce0a5e0e8ba67b455257a3a02a1d99005f382748789d6464460028810f1627

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN apk add --no-cache build-base lld clang clang-extra-tools bash curl tar \
    && curl -fsSL https://github.com/tamasfe/taplo/releases/latest/download/taplo-linux-$(uname -m).gz \
        | gzip -d - | install -m 755 /dev/stdin /usr/local/bin/taplo \
    && rustup toolchain install nightly \
        --allow-downgrade \
        --target $(uname -m)-unknown-linux-musl \
        --component rustc-codegen-cranelift-preview
