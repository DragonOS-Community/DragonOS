# 编译前请先设置参数
sys_root=$DRAGONOS_SYSROOT
binutils_path=请填写binutils的路径

# 要安装到的目录
PREFIX=$HOME/opt/dragonos-host-userspace


if [ ! -d ${binutils_path} ]; then
    echo "Error: ${binutils_path} not found"
    exit 1
fi

if [ ! -d ${sys_root} ]; then
    echo "Error: ${sys_root} not found"
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
${binutils_path}/configure --prefix=${PREFIX} --target=x86_64-dragonos --with-sysroot=${sys_root} --disable-werror || exit 1
make -j $(nproc) || exit 1
make install || exit 1
make clean || exit 1
rm -rf build-binutils