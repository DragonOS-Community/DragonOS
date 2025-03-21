FROM docker.educg.net/cg/os-contest:20250226

RUN apt update
RUN apt install -y --no-install-recommends \
    bison flex libssl-dev bridge-utils dnsmasq sudo iptables

RUN rustup install nightly-2024-11-05
RUN rustup default nightly-2024-11-05
RUN rustup target add riscv64gc-unknown-none-elf --toolchain nightly-2024-11-05
RUN rustup component add rust-src --toolchain nightly-2024-11-05
RUN rustup component add clippy --toolchain nightly-2024-11-05
RUN rustup component add rustfmt --toolchain nightly-2024-11-05
RUN cargo +nightly-2024-11-05 install cargo-binutils

RUN cargo install --git https://github.com/Samuka007/DADK.git --branch 007/breaking-output-path

ENTRYPOINT ["tini", "--"]