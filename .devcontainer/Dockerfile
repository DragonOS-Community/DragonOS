FROM ubuntu:jammy

# 设置环境变量
ENV TZ=Asia/Shanghai
ENV RUSTUP_DIST_SERVER=https://static.rust-lang.org
ENV RUSTUP_UPDATE_ROOT=https://static.rust-lang.org/rustup
ENV FORCE_UNSAFE_CONFIGURE=1

RUN ln -snf /usr/share/zoneinfo/$TZ /etc/localtime && echo $TZ > /etc/timezone

# 创建dragonos用户并设置sudo免密码
RUN apt update && \
    apt install --no-install-recommends -y \
    ca-certificates curl gnupg wget sudo apt-utils && \
    groupadd -g 1000 dragonos && \
    useradd -m -u 1000 -g 1000 -s /bin/bash dragonos && \
    echo "dragonos ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers

# 设置工作目录
USER dragonos
WORKDIR /tmp

COPY ../tools/*.sh ./

RUN bash bootstrap.sh --default --ci && \
    sudo apt autoremove -q -y && \
    sudo apt clean -q -y && \
    sudo rm -rf /tmp/*

WORKDIR /home/dragonos

CMD ["/bin/bash"]
