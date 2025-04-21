#! /bin/bash

#########################################################################
# 这个脚本用于安装musl交叉编译工具链
# 该脚本会自动下载musl交叉编译工具链，并将其添加到PATH中
#########################################################################

export USE_GITHUB=${USE_GITHUB:=0}



MUSL_GCC_DATE="231114"
MUSL_GCC_VERSION="9.4.0"
MUSL_GCC_X86_64_TAR=
MUSL_GCC_RISCV64_TAR=

LA64_GCC_VERSION="loongarch64-cross-14.2.0"
LA64_GCC_TAR="${LA64_GCC_VERSION}.tar.xz"

MUSL_GCC_X86_64_DOWNLOAD_URL=""
MUSL_GCC_RISCV64_DOWNLOAD_URL=""
LA64_GCC_DOWNLOAD_URL="https://mirrors.dragonos.org.cn/pub/third_party/toolchain/gcc/${LA64_GCC_TAR}"

if [ $USE_GITHUB -eq 1 ]; then
    echo "Download from github"

    MUSL_GCC_X86_64_TAR=x86_64-linux-musl-cross-gcc-${MUSL_GCC_VERSION}.tar.xz
    MUSL_GCC_RISCV64_TAR=riscv64-linux-musl-cross-gcc-${MUSL_GCC_VERSION}.tar.xz
    MUSL_GCC_X86_64_DOWNLOAD_URL="https://github.com/DragonOS-Community/musl-cross-make/releases/download/${MUSL_GCC_VERSION}-${MUSL_GCC_DATE}/${MUSL_GCC_X86_64_TAR}"
    MUSL_GCC_RISCV64_DOWNLOAD_URL="https://github.com/DragonOS-Community/musl-cross-make/releases/download/${MUSL_GCC_VERSION}-${MUSL_GCC_DATE}/${MUSL_GCC_RISCV64_TAR}"
else
    echo "Download from mirrors.dragonos.org.cn"
    MUSL_GCC_X86_64_TAR="x86_64-linux-musl-cross-gcc-${MUSL_GCC_VERSION}-${MUSL_GCC_DATE}.tar.xz"
    MUSL_GCC_RISCV64_TAR="riscv64-linux-musl-cross-gcc-${MUSL_GCC_VERSION}-${MUSL_GCC_DATE}.tar.xz"
    MUSL_GCC_X86_64_DOWNLOAD_URL="https://mirrors.dragonos.org.cn/pub/third_party/toolchain/gcc/${MUSL_GCC_X86_64_TAR}"
    MUSL_GCC_RISCV64_DOWNLOAD_URL="https://mirrors.dragonos.org.cn/pub/third_party/toolchain/gcc/${MUSL_GCC_RISCV64_TAR}"
fi


INSTALL_POS="$HOME/opt/"

mkdir -p $INSTALL_POS

get_shell_rc_file()
{
    if [ -n "$ZSH_VERSION" ]; then
        echo "$HOME/.zshrc"
    elif [ -n "$BASH_VERSION" ]; then
        echo "$HOME/.bashrc"
    else
        echo "$HOME/.profile"
    fi
}

# 信号退出时清理下载的文件
trap_handler(){
    rm -f $MUSL_GCC_X86_64_TAR
    rm -f $MUSL_GCC_RISCV64_TAR
    rm -f $LA64_GCC_TAR
}

trap trap_handler EXIT
trap trap_handler SIGINT


SHELL_RC=$(get_shell_rc_file)
source $SHELL_RC

install_loongarch64_gcc()
{
	echo "正在安装 loongarch64-unknown-linux-gnu 工具链"
    
	wget ${LA64_GCC_DOWNLOAD_URL} || exit 1
    echo "正在解压 loongarch64-unknown-linux-gnu 工具链"
    tar xf $LA64_GCC_TAR -C $INSTALL_POS || exit 1
    echo "正在将 loongarch64-unknown-linux-gnu 工具链添加到 PATH 环境变量中"
    echo "export PATH=\$PATH:$INSTALL_POS/${LA64_GCC_VERSION}/bin" >> $SHELL_RC

    echo "loongarch64-unknown-linux-gnu 工具链已成功安装！请运行 source $SHELL_RC 以使更改生效！"
    rm -rf $LA64_GCC_TAR || exit 1
}

# 下载musl交叉编译工具链

# 如果x86_64-linux-musl-gcc或x86_64-linux-musl-g++不存在，则下载
if [ ! -n "$(which x86_64-linux-musl-gcc)" ] || [ ! -n "$(which x86_64-linux-musl-g++)" ]; then
    echo "开始下载x86_64-linux-musl-gcc"
    wget ${MUSL_GCC_X86_64_DOWNLOAD_URL} || exit 1
    echo "下载完成"
    echo "开始解压x86_64-linux-musl-gcc"
    tar xvf $MUSL_GCC_X86_64_TAR -C $INSTALL_POS || exit 1
    echo "export PATH=\$PATH:$INSTALL_POS/x86_64-linux-musl-cross-gcc-${MUSL_GCC_VERSION}/bin" >> $SHELL_RC
    echo "安装完成"
    echo "开始清理x86_64-linux-musl-gcc的下载缓存"
    rm -rf $MUSL_GCC_X86_64_TAR || exit 1
    echo "清理完成"
else
    echo "x86_64-linux-musl-gcc已经安装"
fi

# 如果riscv64-linux-musl-gcc或riscv64-linux-musl-g++不存在，则下载
if [ ! -n "$(which riscv64-linux-musl-gcc)" ] || [ ! -n "$(which riscv64-linux-musl-g++)" ]; then
    echo "开始下载riscv64-linux-musl-gcc"
    wget ${MUSL_GCC_RISCV64_DOWNLOAD_URL} || exit 1
    echo "下载完成"
    echo "开始解压riscv64-linux-musl-gcc"
    tar xvf $MUSL_GCC_RISCV64_TAR -C $INSTALL_POS || exit 1
    echo "export PATH=\"\$PATH:$INSTALL_POS/riscv64-linux-musl-cross-gcc-${MUSL_GCC_VERSION}/bin\"" >> $SHELL_RC
    echo "安装完成"
    echo "开始清理riscv64-linux-musl-gcc的下载缓存"
    rm -rf $MUSL_GCC_RISCV64_TAR || exit 1
    echo "清理完成"
else
    echo "riscv64-linux-musl-gcc已经安装"
fi

if [ ! -n "$(which loongarch64-unknown-linux-gnu-gcc)" ] || [ ! -n "$(which loongarch64-unknown-linux-gnu-g++)" ]; then
    install_loongarch64_gcc || exit 1
else
    echo "loongarch64-unknown-linux-gnu-gcc已经安装"
fi


source $SHELL_RC

echo "musl交叉编译工具链安装完成，请运行 source $SHELL_RC 以使musl交叉编译工具链在当前窗口生效！"
