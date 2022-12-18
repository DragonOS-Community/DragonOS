#!/bin/bash
grub_dir_i386_efi=arch/i386/efi/grub
grub_dir_i386_legacy=arch/i386/legacy/grub
grub_dir_x86_64_efi=arch/x86_64/efi/grub

mkdir -p ${grub_dir_i386_efi}
mkdir -p ${grub_dir_i386_legacy}
mkdir -p ${grub_dir_x86_64_efi}


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
    wget https://mirrors.tuna.tsinghua.edu.cn/gnu/grub/grub-2.06.tar.xz || exit 1
fi

echo "开始下载grub2.06"

echo "下载完成"
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
./configure --target=i386 --prefix=$(dirname $PWD)/${grub_dir_i386_legacy} || exit 1
make -j $(nproc) || exit 1
make install || exit 1
make clean || exit 1

./configure --target=i386 --with-platform=efi --prefix=$(dirname $PWD)/${grub_dir_i386_efi} ||	exit 1
make -j $(nproc) || exit 1
make install || exit 1
make clean || exit 1

./configure --target=x86_64 --with-platform=efi --prefix=$(dirname $PWD)/${grub_dir_x86_64_efi} || exit 1
make -j $(nproc) || exit 1
make install || exit 1

cd ..
#解除权限限制
sudo chmod -R 777 ${grub_dir_i386_legacy}
sudo chmod -R 777 ${grub_dir_i386_efi}
sudo chmod -R 777 ${grub_dir_x86_64_efi}
rm -rf grub-2.06
rm grub-2.06.tar.xz*
echo "grub2.06安装完成"
