FROM docker.educg.net/cg/os-contest:20250226

RUN apt install -y --no-install-recommends \
    bison flex libssl-dev bridge-utils dnsmasq sudo iptables bpf-linker 

# 为应用程序安装工具链
RUN rustup target add x86_64-unknown-linux-musl --toolchain nightly-2024-11-05-x86_64-unknown-linux-gnu

RUN cargo install --git https://github.com/Samuka007/DADK.git --branch kpartx-docker-support

ENTRYPOINT ["tini", "--"]