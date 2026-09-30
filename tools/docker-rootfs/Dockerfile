ARG BASE_IMAGE=ubuntu:24.04@sha256:4fbb8e6a8395de5a7550b33509421a2bafbc0aab6c06ba2cef9ebffbc7092d90
FROM ${BASE_IMAGE}

ARG DEBIAN_FRONTEND=noninteractive
ARG DOCKER_VERSION=5:29.1.3-1~ubuntu.24.04~noble
ARG CONTAINERD_VERSION=2.2.0-2~ubuntu.24.04~noble

# Use signed package repositories and the distribution's maintained CA bundle.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl iproute2 iptables busybox-static \
    && install -d -m 0755 /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/ubuntu/gpg \
        -o /etc/apt/keyrings/docker.asc \
    && chmod 0644 /etc/apt/keyrings/docker.asc \
    && printf '%s\n' \
        'deb [arch=amd64 signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu noble stable' \
        > /etc/apt/sources.list.d/docker.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        "docker-ce=${DOCKER_VERSION}" "docker-ce-cli=${DOCKER_VERSION}" \
        "containerd.io=${CONTAINERD_VERSION}" \
    && update-alternatives --set iptables /usr/sbin/iptables-nft \
    && update-alternatives --set ip6tables /usr/sbin/ip6tables-nft \
    && test -s /etc/ssl/certs/ca-certificates.crt \
    && docker --version && dockerd --version && containerd --version && runc --version \
    && iptables --version | grep -F '(nf_tables)' \
    && install -d /opt/tests/docker/bundle/rootfs/bin /usr/local/share/docker-validation \
    && cp /bin/busybox /opt/tests/docker/bundle/rootfs/bin/busybox \
    && dpkg-query -W > /usr/local/share/docker-validation/packages.txt \
    && rm -rf /var/lib/apt/lists/*

COPY guest/ /usr/local/bin/
RUN chmod 0755 /usr/local/bin/dragon-docker-*

WORKDIR /root
CMD ["/bin/bash"]
