# 编译前请先设置参数
sys_root=$DRAGONOS_SYSROOT
gmp_path=请填写gmp的路径

# 要安装到的目录
PREFIX=/usr


if [ ! -d ${gmp_path} ]; then
    echo "Error: ${gmp_path} not found"
    exit 1
fi

if [ ! -d ${sysroot} ]; then
    echo "Error: ${sysroot} not found"
    exit 1
fi

mkdir -p build-gmp || exit 1
mkdir -p ${PREFIX} || exit 1

cd build-gmp
${gmp_path}/configure --prefix=${PREFIX} --host=x86_64-dragonos  || exit 1
make -j $(nproc) || exit 1
make DESTDIR=${sys_root} install|| exit 1
make clean
cd ..
rm -rf build-gmp