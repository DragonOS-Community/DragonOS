#!/bin/bash
ABS_PREFIX=/opt/dragonos-grub
grub_dir_i386_efi=${ABS_PREFIX}/arch/i386/efi/grub
grub_dir_i386_legacy=${ABS_PREFIX}/arch/i386/legacy/grub
grub_dir_x86_64_efi=${ABS_PREFIX}/arch/x86_64/efi/grub

sudo mkdir -p ${grub_dir_i386_efi}
sudo mkdir -p ${grub_dir_i386_legacy}
sudo mkdir -p ${grub_dir_x86_64_efi}

# 防止外层声明了环境变量，影响到grub的编译
export CC=gcc
export LD=ld
export AS=as
export NM=nm
export OBJCOPY=objcopy


#检测grub是否已经安装
if [ -d ${grub_dir_i386_efi}/bin ] && [ -d ${grub_dir_i386_legacy}/bin ] && [ -d ${grub_dir_x86_64_efi}/bin ] ; then
	exit 0
fi
#仅支持Ubuntu/Debain下的自动安装
if ! hash 2>/dev/null apt-get; then
	echo "脚本暂不支持对该系统下grub的安装，请手动完成"
	exit 0
fi

#下载grub2.06
if [ ! -f "grub-2.06.tar.xz" ]; then
    echo "开始下载grub2.06"
    wget https://mirrors.ustc.edu.cn/gnu/grub/grub-2.06.tar.xz || exit 1
    echo "下载完成"
fi

tar xvf grub-2.06.tar.xz
#安装对应依赖
sudo apt-get update
sudo apt-get install -y \
	make 	\
  	binutils \
  	bison 	\
  	gcc 	\
  	gettext \
	flex	\
	bison	\
	automake	\
	autoconf	
	
cd grub-2.06
echo "开始安装grub2.06"
#编译安装三个版本的grub
./configure --target=i386 --prefix=${grub_dir_i386_legacy} || exit 1
make -j $(nproc) || exit 1
sudo make install || exit 1
make clean || exit 1

./configure --target=i386 --with-platform=efi --prefix=${grub_dir_i386_efi} ||	exit 1
make -j $(nproc) || exit 1
sudo make install || exit 1
make clean || exit 1

./configure --target=x86_64 --with-platform=efi --prefix=${grub_dir_x86_64_efi} || exit 1
make -j $(nproc) || exit 1
sudo make install || exit 1

cd ..
#解除权限限制
sudo chmod -R 777 ${grub_dir_i386_legacy}
sudo chmod -R 777 ${grub_dir_i386_efi}
sudo chmod -R 777 ${grub_dir_x86_64_efi}
rm -rf grub-2.06
rm grub-2.06.tar.xz*
echo "grub2.06安装完成"
