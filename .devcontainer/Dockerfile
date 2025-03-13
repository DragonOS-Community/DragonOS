From docker.educg.net/cg/os-contest:20250226

RUN apt install -y --no-install-recommends \
    bison flex libssl-dev bridge-utils dnsmasq sudo iptables

RUN cargo install --git https://github.com/Samuka007/DADK.git --branch kpartx-docker-support

ENTRYPOINT ["tini", "--"]