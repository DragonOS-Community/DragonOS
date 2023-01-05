# 编译前请先设置参数
sys_root=/media/longjin/4D0406C21F585A40/2022/DragonOS/bin/sys_root
gcc_path=/media/longjin/4D0406C21F585A40/2022/code/dragonos-gcc

# 要安装到的目录
PREFIX=$HOME/opt/dragonos-userspace-gcc


if [ ! -d ${gcc_path} ]; then
    echo "Error: ${gcc_path} not found"
    exit 1
fi

if [ ! -d ${sysroot} ]; then
    echo "Error: ${sysroot} not found"
    exit 1
fi

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

mkdir -p build-gcc || exit 1
mkdir -p ${PREFIX} || exit 1

cd build-gcc
${gcc_path}/configure --prefix=${PREFIX} --target=x86_64-dragonos --with-sysroot=${sysroot} --disable-werror --enable-languages=c || exit 1
make all-gcc all-target-libgcc -j $(nproc) || exit 1
make install-gcc install-target-libgcc -j $(nproc)  || exit 1
make clean || exit 1
rm -rf build-gcc