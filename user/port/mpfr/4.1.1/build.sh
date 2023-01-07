# 安装命令
# curl https://ftp.gnu.org/gnu/mpfr/mpfr-4.1.1.tar.gz
# tar zxvf mpfr-4.1.1.tar.gz
# rm mpfr-4.1.1.tar.gz

# 编译前请先设置参数
sys_root=$DRAGONOS_SYSROOT
src_path=请填写mpfr的路径
# $HOME/DragonOS/user/port/mpfr/4.1.1/mpfr-4.1.1

current_path=$(pwd)
# 要安装到的目录(相对于sysroot)
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
# sed -i 's/ios[*]/ios* | dragonos* /' config.sub

cd ${current_path}

mkdir -p build || exit 1
mkdir -p ${PREFIX} || exit 1

cd build
${src_path}/configure --prefix=${PREFIX} --host=x86_64-dragonos  || exit 1
make -j $(nproc) || exit 1
make DESTDIR=${sys_root} install|| exit 1
make clean
cd ..
rm -rf build