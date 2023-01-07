# 从flex仓库拉取对应版本的flex
# curl https://github.com/westes/flex/files/981163/flex-2.6.4.tar.gz
# tar zxvf flex-2.6.4.tar.gz
# rm flex-2.6.4.tar.gz

# 编译前请先设置参数
sys_root=$DRAGONOS_SYSROOT
src_path=请填写flex的路径
# $HOME/DragonOS/user/port/flex/2.6.4/flex-2.6.4

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

# 配置步骤：
# 1. 找到config.sub文件，找到 accept the basic system types，在gnu*后面加入dragonos
# 2. 

cd ${src_path}
autoreconf --install
autoconf
# sed -i 's/gnu*/gnu* | dragonos*/' build-aux/config.sub

cd ${current_path}

mkdir -p build-flex || exit 1
mkdir -p ${PREFIX} || exit 1

cd build-flex
bash ${src_path}/autogen.sh
${src_path}/configure --prefix=${PREFIX} --host=x86_64-dragonos || exit 1
make -j $(nproc) || exit 1
make DESTDIR=${sys_root} install|| exit 1
make clean
cd ..
rm -rf build