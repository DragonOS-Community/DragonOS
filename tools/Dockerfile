FROM ubuntu:jammy

# 设置用来存放rust缓存的卷
VOLUME [ "/home/dragonos/.cargo/registry" ]
VOLUME [ "/home/dragonos/.cargo/git" ]

# 设置环境变量，定义用户和用户组
ENV NEW_USER=dragonos
ENV NEW_GROUP=dragonos
ENV NEW_UID=1000
ENV NEW_GID=1000
ENV TZ=Asia/Shanghai
RUN ln -snf /usr/share/zoneinfo/$TZ /etc/localtime && echo $TZ > /etc/timezone

# 创建用户组
RUN groupadd --gid $NEW_GID $NEW_GROUP

# 创建用户并设置ID
RUN useradd --uid $NEW_UID --gid $NEW_GID --create-home --shell /bin/bash $NEW_USER

# 切换到新创建的用户

# 设置工作目录
WORKDIR /tmp

# 将本地的脚本复制到工作目录
COPY *.sh ./

# 设置sudo免密码
RUN mkdir -p /etc/sudoers.d && \
    echo "${NEW_USER} ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers.d/${NEW_USER} && \
    chmod 0440 /etc/sudoers.d/${NEW_USER} && \
    chown -R dragonos:dragonos /home/dragonos && \
    apt update && \
    apt install -y ca-certificates curl gnupg wget sudo apt-utils && \
    sudo apt clean -q -y

USER $NEW_USER

RUN touch /home/${NEW_USER}/.bashrc && \
    sudo chown -R ${NEW_USER}:${NEW_GROUP} /home/${NEW_USER}/.cargo/ && \
    bash bootstrap.sh --default && \
    sudo cp /tmp/docker-entrypoint.sh /home/dragonos/entrypoint.sh && \
    sudo chown ${NEW_USER}:${NEW_GROUP} /home/dragonos/entrypoint.sh && \
    sudo chmod a+rwx /home/dragonos/entrypoint.sh && \
    sudo apt autoremove -q -y && \         
    sudo apt clean -q -y && \
    sudo rm -rf /tmp/*

WORKDIR /home/dragonos

ENTRYPOINT [ "/home/dragonos/entrypoint.sh" ]
# 设置容器启动后执行的命令
CMD ["/bin/bash"]
