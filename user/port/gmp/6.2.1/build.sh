# 下载安装gmp-v6.2.1
# wget https://gmplib.org/download/gmp/gmp-6.2.1.tar.xz 
# tar -xvJf gmp-6.2.1.tar.xz
# rm gmp-6.2.1.tar.xz

# 编译前请先设置参数
sys_root=$DRAGONOS_SYSROOT
gmp_path=请填写gmp的路径
# $HOME/DragonOS/user/port/gmp/6.2.1/gmp-6.2.1

# 要安装到的目录(相对于sysroot)
PREFIX=/usr


if [ ! -d ${gmp_path} ]; then
    echo "Error: ${gmp_path} not found"
    exit 1
fi

if [ ! -d ${sysroot} ]; then
    echo "Error: ${sysroot} not found"
    exit 1
fi

# 配置步骤：
# 1. 找到configfsf.sub文件，找到Now accept the basic system types，在gnu*后面加入dragonos
# 2. 

mkdir -p build-gmp || exit 1
mkdir -p ${PREFIX} || exit 1

# sed -i 's/gnu*/gnu* | dragonos /' gmp-6.2.1/configfsf.sub

cd build-gmp
${gmp_path}/configure --prefix=${PREFIX} --host=x86_64-dragonos  || exit 1
make -j $(nproc) || exit 1
make DESTDIR=${sys_root} install|| exit 1
make clean
cd ..
rm -rf build-gmp