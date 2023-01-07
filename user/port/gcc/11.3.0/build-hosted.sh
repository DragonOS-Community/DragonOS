##############################################
# DragonOS hosted gcc build script
#
# This script is used to build userland gcc for DragonOS（Running on Linux)
##############################################

# 编译前请先设置参数
sys_root=$DRAGONOS_SYSROOT
gcc_path=请填写gcc的路径
# $HOME/opt/gcc 

# 要安装到的目录
PREFIX=$HOME/opt/dragonos-host-userspace


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
${gcc_path}/configure --prefix=${PREFIX} --target=x86_64-dragonos --with-sysroot=${sys_root} --disable-werror --disable-shared --disable-bootstrap --enable-languages=c,c++ || exit 1
make all-gcc all-target-libgcc -j $(nproc) || exit 1
make install-gcc install-target-libgcc -j $(nproc)  || exit 1
# 这里会报错，暂时不知道为什么
# make all-target-libstdc++-v3 -j $(nproc) || exit 1
# make install-target-libstdc++-v3 -j $(nproc) || exit 1
make clean
cd ..
rm -rf build-gcc
