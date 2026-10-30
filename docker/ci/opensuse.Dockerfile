FROM opensuse/tumbleweed:latest@sha256:d95ddcc0ae62a9df3e6e424cb39a00ba74ef1744b02d7914bdbec4c7657baf01

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN zypper refresh \
    && zypper in -y gcc gcc-c++ glibc-devel-static clang lld llvm-gold curl rustup \
    && zypper clean --all \
    && curl -fsSL https://github.com/tamasfe/taplo/releases/latest/download/taplo-linux-$(uname -m).gz \
        | gzip -d - | install -m 755 /dev/stdin /usr/local/bin/taplo \
    && rustup toolchain install nightly \
        --allow-downgrade \
        --target $(uname -m)-unknown-linux-musl \
        --component rustc-codegen-cranelift-preview
