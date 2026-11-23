ARG BASE_IMAGE=ubuntu:24.04@sha256:4fbb8e6a8395de5a7550b33509421a2bafbc0aab6c06ba2cef9ebffbc7092d90
FROM ${BASE_IMAGE}

ARG DEBIAN_FRONTEND=noninteractive
ARG DOCKER_VERSION=5:29.1.3-1~ubuntu.24.04~noble
ARG CONTAINERD_VERSION=2.2.0-2~ubuntu.24.04~noble
# Official Node.js runtime. Ubuntu noble only ships nodejs 18, so take the
# upstream release tarball (pinned version + checksum) instead.
ARG NODE_VERSION=v24.22.3
ARG NODE_SHA256=4f2b175a6d53a5c617a80dba2676238a0688cef28c6da4eba35c7d844e66fcfb
ARG NODE_DIST_URL=https://nodejs.org/dist/${NODE_VERSION}/node-${NODE_VERSION}-linux-x64.tar.xz
# Debian/Ubuntu archive mirror. USTC is the project's default (the guest rcS
# switches to it as well); override with --build-arg for other regions.
ARG APT_MIRROR=http://mirrors.ustc.edu.cn/ubuntu

# Use signed package repositories and the distribution's maintained CA bundle.
RUN sed -i "s|http://archive.ubuntu.com/ubuntu|${APT_MIRROR}|g; s|http://security.ubuntu.com/ubuntu|${APT_MIRROR}|g" \
        /etc/apt/sources.list.d/ubuntu.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl iproute2 iptables busybox-static xz-utils \
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
    && apt-get install -y --no-install-recommends python3 python-is-python3 \
    && curl -fsSL --retry 3 -o /tmp/node.tar.xz "${NODE_DIST_URL}" \
    && echo "${NODE_SHA256}  /tmp/node.tar.xz" | sha256sum -c - \
    && tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1 \
    && rm -f /tmp/node.tar.xz \
    && ln -sf /usr/local/bin/node /usr/bin/node \
    && ln -sf /usr/local/bin/npm /usr/bin/npm \
    && ln -sf /usr/local/bin/npx /usr/bin/npx \
    && python3 --version && node --version && npm --version \
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
