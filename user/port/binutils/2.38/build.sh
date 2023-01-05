# 编译前请先设置参数
sys_root=/media/longjin/4D0406C21F585A40/2022/DragonOS/bin/sys_root
binutils_path=/media/longjin/4D0406C21F585A40/2022/code/dragonos-binutils-gdb

# 要安装到的目录
PREFIX=$HOME/opt/dragonos-userspace-binutils


if [ ! -d ${binutils_path} ]; then
    echo "Error: ${binutils_path} not found"
    exit 1
fi

if [ ! -d ${sysroot} ]; then
    echo "Error: ${sysroot} not found"
    exit 1
fi


mkdir -p build-binutils || exit 1
mkdir -p ${PREFIX} || exit 1

# 安装依赖
# 注意texinfo和binutils的版本是否匹配
# 注意gmp/mpc/mpfr和gcc/g++的版本是否匹配
sudo apt-get install -y \
    g++ \
    gcc \
    make \
    texinfo \
    libgmp3-dev \
    libmpc-dev \
    libmpfr-dev \
    flex \
    wget

cd build-binutils
${binutils_path}/configure --prefix=${PREFIX} --target=x86_64-dragonos --with-sysroot=${sysroot} --disable-werror || exit 1
make -j $(nproc) || exit 1
make install || exit 1
make clean || exit 1
rm -rf build-binutils