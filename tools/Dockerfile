FROM ubuntu:jammy

# 设置环境变量
ENV TZ=Asia/Shanghai
RUN ln -snf /usr/share/zoneinfo/$TZ /etc/localtime && echo $TZ > /etc/timezone

# 设置工作目录
WORKDIR /tmp

# 将本地的脚本复制到工作目录
COPY *.sh ./

# 设置sudo免密码
RUN apt update && \
    apt install -y ca-certificates curl gnupg wget sudo apt-utils && \
    bash bootstrap.sh --default && \
    sudo cp /tmp/docker-entrypoint.sh /root/entrypoint.sh && \
    sudo chmod a+rwx /root/entrypoint.sh && \
    sudo apt autoremove -q -y && \         
    sudo apt clean -q -y && \
    sudo rm -rf /tmp/*

WORKDIR /root

ENTRYPOINT [ "/root/entrypoint.sh" ]
# 设置容器启动后执行的命令
CMD ["/bin/bash"]
