
# 编译前请先设置参数
sys_root=/media/longjin/4D0406C21F585A40/2022/DragonOS/bin/sysroot

src_path=/media/longjin/4D0406C21F585A40/2022/code/dragonos-flex
current_path=$(pwd)
# 要安装到的目录
PREFIX=/usr


if [ ! -d ${src_path} ]; then
    echo "Error: ${src_path} not found"
    exit 1
fi

if [ ! -d ${sysroot} ]; then
    echo "Error: ${sysroot} not found"
    exit 1
fi

cd ${src_path}
autoreconf --install
autoconf
sed -i 's/ios[*]/ios* | dragonos* /' build-aux/config.sub

cd ${current_path}

mkdir -p build || exit 1
mkdir -p ${PREFIX} || exit 1

cd build
${src_path}/configure --prefix=${PREFIX} --host=x86_64-dragonos || exit 1
make -j $(nproc) || exit 1
make DESTDIR=${sys_root} install|| exit 1
make clean
cd ..
rm -rf build