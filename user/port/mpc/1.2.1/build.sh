# 编译前请先设置参数
sys_root=$DRAGONOS_SYSROOT
mpc_path=请填写mpc的路径

# 要安装到的目录
PREFIX=/usr
current_path=$(pwd)

if [ ! -d ${mpc_path} ]; then
    echo "Error: ${mpc_path} not found"
    exit 1
fi

if [ ! -d ${sysroot} ]; then
    echo "Error: ${sysroot} not found"
    exit 1
fi

cd ${mpc_path}
autoreconf --install || exit 1
autoconf
sed -i 's/ios[*]/ios* | dragonos* /' build-aux/config.sub

cd ${current_path}

mkdir -p build || exit 1
mkdir -p ${PREFIX} || exit 1

cd build
${mpc_path}/configure --prefix=${PREFIX} --host=x86_64-dragonos --target=x86_64-dragonos --with-mpfr=$sys_root/usr --with-gmp=$sys_root/usr || exit 1
make -j $(nproc) || exit 1
make DESTDIR=${sys_root} install || exit 1
make clean
cd ..
rm -rf build