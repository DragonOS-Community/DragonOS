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
#仅支持Ubuntu/Debain, Arch下的自动安装
supported_package_manager="apt-get pacman"
packages=("make binutils bison gcc gettext flex bison automake autoconf wget" \
          "make binutils bison gcc gettext flex bison automake autoconf wget")
update_options=("update" \
                "-Sy")
install_options=("install -y" \
                 "-S --needed --noconfirm")
found_pm=0
pm_index=0
for pm in ${supported_package_manager}; do
    if hash 2>/dev/null ${pm}; then
        found_pm=1
        break
    fi
    let pm_index=$pm_index+1
done
if [ ${found_pm} = "1" ]; then
	echo "found package manager: ${pm}"
else
	echo "找不到任何支持的包管理器: ${supported_package_manager}"
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
sudo ${pm} ${update_options[$pm_index]}
sudo ${pm} ${install_options[$pm_index]} ${packages[$pm_index]}
	
cd grub-2.06
echo "开始安装grub2.06"
#编译安装三个版本的grub
./configure --target=i386 --prefix=${grub_dir_i386_legacy} --disable-werror || exit 1
make -j $(nproc) || exit 1
sudo make install || exit 1
make clean || exit 1

./configure --target=i386 --with-platform=efi --prefix=${grub_dir_i386_efi} --disable-werror ||	exit 1
make -j $(nproc) || exit 1
sudo make install || exit 1
make clean || exit 1

./configure --target=x86_64 --with-platform=efi --prefix=${grub_dir_x86_64_efi} --disable-werror || exit 1
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
