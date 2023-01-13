# 本Dockerfile用于构建binutils 2.38的交叉编译环境

FROM ubuntu:jammy

# Install dependencies
RUN apt-get update && apt-get install -y \
    autoconf2.69 \
    automake \
    bison \
    build-essential \
    flex \
    gawk \
    gettext \
    git \
    libgmp-dev \
    libmpc-dev \
    libmpfr-dev \
    libncurses5-dev \
    libtool \
    m4 \
    make \
    ninja-build \
    python3 \
    texinfo \
    wget \
    xz-utils \
    zlib1g-dev \
    wget \
    && rm /usr/bin/autoconf && ln -s /usr/bin/autoconf2.69 /usr/bin/autoconf

WORKDIR /opt

# download automake 1.15.1
RUN wget http://mirrors.ustc.edu.cn/gnu/automake/automake-1.15.1.tar.xz && \
    tar -xvf automake-1.15.1.tar.xz && \
    cd automake-1.15.1 && \
    ./configure --prefix=/usr && \
    make && \
    make install && \
    cd .. && \
    rm -rf automake-1.15.1 automake-1.15.1.tar.xz
