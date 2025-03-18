#########################################################################
# 这个脚本用于安装musl交叉编译工具链
# 该脚本会自动下载musl交叉编译工具链，并将其添加到PATH中
#########################################################################

export USE_GITHUB=${USE_GITHUB:=0}



MUSL_GCC_DATE="231114"
MUSL_GCC_VERSION="9.4.0"
MUSL_GCC_X86_64_TAR=
MUSL_GCC_RISCV64_TAR=

MUSL_GCC_X86_64_DOWNLOAD_URL=""
MUSL_GCC_RISCV64_DOWNLOAD_URL=""
if [ $USE_GITHUB -eq 1 ]; then
    echo "Download from github"

    MUSL_GCC_X86_64_TAR=x86_64-linux-musl-cross-gcc-${MUSL_GCC_VERSION}.tar.xz
    MUSL_GCC_RISCV64_TAR=riscv64-linux-musl-cross-gcc-${MUSL_GCC_VERSION}.tar.xz
    MUSL_GCC_X86_64_DOWNLOAD_URL="https://github.com/DragonOS-Community/musl-cross-make/releases/download/${MUSL_GCC_VERSION}-${MUSL_GCC_DATE}/${MUSL_GCC_X86_64_TAR}"
    MUSL_GCC_RISCV64_DOWNLOAD_URL="https://github.com/DragonOS-Community/musl-cross-make/releases/download/${MUSL_GCC_VERSION}-${MUSL_GCC_DATE}/${MUSL_GCC_RISCV64_TAR}"
    https://github.com/DragonOS-Community/musl-cross-make/releases/download/9.4.0-231114/riscv64-linux-musl-cross-gcc-9.4.0.tar.xz
else
    echo "Download from mirrors.dragonos.org.cn"
    MUSL_GCC_X86_64_TAR="x86_64-linux-musl-cross-gcc-${MUSL_GCC_VERSION}-${MUSL_GCC_DATE}.tar.xz"
    MUSL_GCC_RISCV64_TAR="riscv64-linux-musl-cross-gcc-${MUSL_GCC_VERSION}-${MUSL_GCC_DATE}.tar.xz"
    MUSL_GCC_X86_64_DOWNLOAD_URL="https://mirrors.dragonos.org.cn/pub/third_party/toolchain/gcc/${MUSL_GCC_X86_64_TAR}"
    MUSL_GCC_RISCV64_DOWNLOAD_URL="https://mirrors.dragonos.org.cn/pub/third_party/toolchain/gcc/${MUSL_GCC_RISCV64_TAR}"
fi


INSTALL_POS="/opt"

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
}

trap trap_handler EXIT
trap trap_handler SIGINT


SHELL_RC=$(get_shell_rc_file)
source $SHELL_RC

# 下载musl交叉编译工具链

# 如果x86_64-linux-musl-gcc或x86_64-linux-musl-g++不存在，则下载
if [ ! -n "$(which x86_64-linux-musl-gcc)" ] || [ ! -n "$(which x86_64-linux-musl-g++)" ]; then
    echo "开始下载x86_64-linux-musl-gcc"
    wget ${MUSL_GCC_X86_64_DOWNLOAD_URL} || exit 1
    echo "下载完成"
    echo "开始解压x86_64-linux-musl-gcc"
    tar xvf $MUSL_GCC_X86_64_TAR -C $INSTALL_POS || exit 1
    echo "PATH=\$PATH:$INSTALL_POS/x86_64-linux-musl-cross-gcc-${MUSL_GCC_VERSION}/bin" >> $SHELL_RC
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

source $SHELL_RC

echo "musl交叉编译工具链安装完成，请运行 source $SHELL_RC 以使musl交叉编译工具链在当前窗口生效！"
