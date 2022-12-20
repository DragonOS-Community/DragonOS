FROM ubuntu:jammy

# 设置工作目录
WORKDIR /build-image

# 将本地的脚本复制到工作目录
COPY *.sh ./

# 设置用来存放rust缓存的卷
VOLUME [ "/root/.cargo/registry" ]

# 设置rust源的环境变量
ENV RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static
ENV RUSTUP_UPDATE_ROOT=https://mirrors.ustc.edu.cn/rust-static/rustup

RUN apt update && apt install -y \
    ca-certificates \
    curl \
    gnupg \
    lsb-release \
    llvm-dev libclang-dev clang gcc-multilib \
    gcc build-essential fdisk dosfstools qemu-kvm \
    sudo wget

# 安装Rust
RUN curl https://sh.rustup.rs -sSf | sh -s -- --default-toolchain nightly -y && \
    # You have to add the rustup variables to the $PATH 
    echo "export PATH=\"\$HOME/.cargo/bin:\$PATH\"" >> ~/.bashrc && \
    bash -c "source $HOME/.cargo/env && \
    # 更换cargo的索引源
    touch $HOME/.cargo/config && \
    bash change_rust_src.sh && \
    # 安装rust的编译工具
    echo "正在安装DragonOS所需的rust组件...首次安装需要一些时间来更新索引，请耐心等待..." && \
    cargo install cargo-binutils && \
    rustup toolchain install nightly && \
    rustup default nightly && \
    rustup component add rust-src && \
    rustup component add llvm-tools-preview && \
    rustup target add x86_64-unknown-none"

# 编译安装GCC交叉编译工具链
RUN bash build_gcc_toolchain.sh -f

# 安装GRUB2.06
RUN bash grub_auto_install.sh

# 清除缓存
WORKDIR /
RUN apt-get autoremove -q -y && \         
    apt-get clean -q -y && \
    rm -rf /build-image

ENV DragonOS_GCC=/root/opt/dragonos-gcc/gcc-x86_64-unknown-none/bin/
# 设置容器启动后执行的命令
CMD ["/bin/bash"]
