# 编译前请先设置参数
sysroot=请在这里输入sysroot的绝对路径，就是:DragonOS项目的目录的绝对路径/bin/sysroot
binutils_path=请在这里输入DragonOS-binutils的绝对路径


if [ ! -d ${binutils_path} ]; then
    echo "Error: ${binutils_path} not found"
    exit 1
fi

if [ ! -d ${sysroot} ]; then
    echo "Error: ${sysroot} not found"
    exit 1
fi

PREFIX=$(pwd)/build-binutils/install

mkdir -p build-binutils || exit 1
mkdir -p ${PREFIX} || exit 1

cd build-binutils
${binutils_path}/configure --prefix=${PREFIX} --target=x86_64-dragonos --with-sysroot=${sysroot} --disable-werror || exit 1
make -j $(nproc) || exit 1
make install || exit 1
make clean