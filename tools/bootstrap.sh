#!/bin/bash

if test -n "$ZSH_VERSION"; then
  CURRENT_SHELL=zsh
elif test -n "$BASH_VERSION"; then
  CURRENT_SHELL=bash
elif test -n "$KSH_VERSION"; then
  CURRENT_SHELL=ksh
elif test -n "$FCEDIT"; then
  CURRENT_SHELL=ksh
elif test -n "$PS3"; then
  CURRENT_SHELL=unknown
else
  CURRENT_SHELL=sh
fi

source "$HOME/.$CURRENT_SHELL"rc

emulator="qemu"
defpackman="apt-get"
dockerInstall="true"
export RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static
export RUSTUP_UPDATE_ROOT=https://mirrors.ustc.edu.cn/rust-static/rustup

banner()
{
	echo "|------------------------------------------|"
	echo "|    Welcome to the DragonOS bootstrap     |"
	echo "|------------------------------------------|"
}

# 因为编码原因, 只有在vim打开该文件的时候对齐才是真的对齐
congratulations()
{
	echo "|-----------Congratulations!---------------|"
	echo "|                                          |"
	echo "|   你成功安装了DragonOS所需的依赖项!      |"
    echo "|                                          |"
    echo "|   请[关闭]当前终端, 并[重新打开]一个终端 |"
	echo "|   然后通过以下命令运行:                  |"
	echo "|                                          |"
	echo "|                make run                  |"
	echo "|                                          |"
	echo "|------------------------------------------|"
}

####################################
# 当检测到ubuntu或Debian时，执行此函数 #
# 参数:第一个参数为包管理器            #
####################################
install_ubuntu_debian_pkg()
{
    echo "检测到 Ubuntu/Debian"
	echo "正在更新包管理器的列表..."
	sudo "$1" update
	echo "正在安装所需的包..."
    sudo "$1" install -y \
        ca-certificates \
        curl wget \
        unzip \
        gnupg \
        lsb-release \
        llvm-dev libclang-dev clang gcc-multilib \
        gcc build-essential fdisk dosfstools dnsmasq bridge-utils iptables libssl-dev pkg-config \
		sphinx
	# 必须分开安装，否则会出现错误
	sudo "$1" install -y \
		gcc-riscv64-unknown-elf gcc-riscv64-linux-gnu gdb-multiarch
	
	# 如果python3没有安装
	if [ -z "$(which python3)" ]; then
		echo "正在安装python3..."
		sudo apt install -y python3 python3-pip
	fi

    if [ -z "$(which docker)" ] && [ -n ${dockerInstall} ]; then
        echo "正在安装docker..."
        sudo apt install -y docker.io docker-compose
		sudo groupadd docker
		sudo usermod -aG docker $USER
		sudo systemctl restart docker
    elif [ -z ${dockerInstall} ]; then
		echo "您传入--no-docker参数生效, 安装docker步骤被跳过."
	elif [ -n "$(which docker)" ]; then
        echo "您的计算机上已经安装了docker"
    fi

    if [ -z "$(which qemu-system-x86_64)" ]; then
        echo "正在安装QEMU虚拟机..."
        sudo $1 install -y qemu-system qemu-kvm
    else
        echo "QEMU已经在您的电脑上安装！"
    fi

}

install_archlinux_pkg()
{
    pkgman="pacman"
    echo "检测到 ArchLinux"
    echo "正在更新包管理器的列表..."
    sudo "${pkgman}" -Sy
    echo "正在安装所需的包..."
    sudo "${pkgman}" -S --needed --noconfirm \
	curl wget bridge-utils dnsmasq \
        diffutils pkgconf which unzip util-linux dosfstools \
        gcc make flex texinfo gmp mpfr qemu-base \
        libmpc openssl

}

install_osx_pkg()
{
    echo "Detected OSX! 暂不支持Mac OSX的一键安装！"
    exit 1
}

####################################################################################
# This function takes care of everything associated to rust, and the version manager
# That controls it, it can install rustup and uninstall multirust as well as making
# sure that the correct version of rustc is selected by rustup
####################################################################################
rustInstall() {
	# Check to see if multirust is installed, we don't want it messing with rustup
	# In the future we can probably remove this but I believe it's good to have for now
	if [ -e /usr/local/lib/rustlib/uninstall.sh ] ; then
		echo "您的系统上似乎安装了multirust。"
		echo "该工具已被维护人员弃用，并将导致问题。"
		echo "如果您愿意，此脚本可以从系统中删除multirust"
		printf "卸载 multirust (y/N):"
		read multirust
		if echo "$multirust" | grep -iq "^y" ;then
			sudo /usr/local/lib/rustlib/uninstall.sh
		else
			echo "请手动卸载multistrust和任何其他版本的rust，然后重新运行bootstrap.sh"
			exit
		fi
	fi
	# If rustup is not installed we should offer to install it for them
	if [ -z "$(which rustup)" ]; then
		echo "您没有安装rustup,"
		echo "我们强烈建议使用rustup, 是否要立即安装？"
		echo "*WARNING* 这将会发起这样的一个命令 'curl | sh' "
		printf "(y/N): "
		read rustup
		if echo "$rustup" | grep -iq "^y" ;then
			#install rustup
			curl https://sh.rustup.rs -sSf | sh -s -- --default-toolchain nightly
			# You have to add the rustup variables to the $PATH
			echo "export PATH=\"\$HOME/.cargo/bin:\$PATH\"" >> ~/.bashrc
			# source the variables so that we can execute rustup commands in the current shell
			source ~/.cargo/env
			source "$HOME/.cargo/env"
		else
			echo "Rustup will not be installed!"
		fi
	fi
	#
	if [ -z "$(which rustc)" ]; then
		echo "Rust 还未被安装"
		echo "请再次运行脚本，接受rustup安装"
		echo "或通过以下方式手动安装rustc（不推荐）："
		echo "curl https://sh.rustup.rs -sSf | sh -s -- --default-toolchain nightly"
		exit
	else
        echo "是否为Rust换源为国内镜像源？(Tuna)"
		echo "如果您在国内，我们推荐您这样做，以提升网络速度。"
		echo "*WARNING* 这将会替换原有的镜像源设置。"
		printf "(y/N): "
		read change_src
		if echo "$change_src" | grep -iq "^y" ;then
			touch ~/.cargo/config
			bash change_rust_src.sh
		else
			echo "取消换源，您原有的配置不会被改变。"
		fi
        echo "正在安装DragonOS所需的rust组件...首次安装需要一些时间来更新索引，请耐心等待..."
        cargo install cargo-binutils
		rustup toolchain install nightly-2023-01-21-x86_64-unknown-linux-gnu
		rustup toolchain install nightly-2023-08-15-x86_64-unknown-linux-gnu
		rustup component add rust-src --toolchain nightly-2023-01-21-x86_64-unknown-linux-gnu
		rustup component add rust-src --toolchain nightly-2023-08-15-x86_64-unknown-linux-gnu
		rustup target add x86_64-unknown-none --toolchain nightly-2023-01-21-x86_64-unknown-linux-gnu
		rustup target add x86_64-unknown-none --toolchain nightly-2023-08-15-x86_64-unknown-linux-gnu
		rustup target add x86_64-unknown-linux-musl --toolchain nightly-2023-08-15-x86_64-unknown-linux-gnu

		rustup toolchain install nightly-2023-01-21-riscv64gc-unknown-linux-gnu --force-non-host
		rustup toolchain install nightly-2023-08-15-riscv64gc-unknown-linux-gnu --force-non-host
		rustup target add riscv64gc-unknown-none-elf --toolchain nightly-2023-01-21-riscv64gc-unknown-linux-gnu
		rustup target add riscv64imac-unknown-none-elf --toolchain nightly-2023-01-21-riscv64gc-unknown-linux-gnu
		rustup target add riscv64gc-unknown-none-elf --toolchain nightly-2023-08-15-riscv64gc-unknown-linux-gnu
		rustup target add riscv64imac-unknown-none-elf --toolchain nightly-2023-08-15-riscv64gc-unknown-linux-gnu
        
		rustup component add rust-src --toolchain nightly-x86_64-unknown-linux-gnu
		rustup component add rust-src
        rustup component add llvm-tools-preview
		rustup default nightly
		
		echo "Rust已经成功的在您的计算机上安装！请运行 source ~/.cargo/env 以使rust在当前窗口生效！"
	fi
}

####################################################################################
# 初始化DragonOS的musl交叉编译工具链
# 主要是把musl交叉编译工具链的rcrt1.o替换为crt1.o (因为rust的rcrt1.o会使用动态链接的解释器，但是DragonOS目前尚未把它加载进来)
#
# 为DragonOS开发应用的时候，请使用 `cargo +nightly-2023-08-15-x86_64-unknown-linux-gnu build --target x86_64-unknown-linux-musl` 来编译
# 	这样编译出来的应用将能二进制兼容DragonOS 
####################################################################################
initialize_userland_musl_toolchain()
{
	fork_toolchain_from="nightly-2023-08-15-x86_64-unknown-linux-gnu"
	custom_toolchain="nightly-2023-08-15-x86_64-unknown-linux_dragonos-gnu"
	custom_toolchain_dir="$(dirname $(rustc --print sysroot))/${custom_toolchain}"
	# 如果目录为空
	if [ ! -d "${custom_toolchain_dir}" ]; then
		echo "Custom toolchain does not exist, creating..."
		rustup toolchain install ${fork_toolchain_from}
		rustup component add --toolchain ${fork_toolchain_from} rust-src
		rustup target add --toolchain ${fork_toolchain_from} x86_64-unknown-linux-musl
		cp -r $(dirname $(rustc --print sysroot))/${fork_toolchain_from} ${custom_toolchain_dir}
		self_contained_dir=${custom_toolchain_dir}/lib/rustlib/x86_64-unknown-linux-musl/lib/self-contained
		cp -f ${self_contained_dir}/crt1.o ${self_contained_dir}/rcrt1.o
	else
		echo "Custom toolchain already exists."
	fi

}


install_python_pkg()
{
	echo "正在安装python依赖项..."
	# 安装文档生成工具
	sh -c "cd ../docs && pip3 install -r requirements.txt"
}


############# 脚本开始 ##############
# 读取参数及选项，使用 -help 参数查看详细选项
while true; do
	if [ -z "$1" ]; then
		break;
	fi
	echo "repeat"
	case "$1" in
		"--no-docker")
			dockerInstall=""
		;;
		"--help")
			echo "--no-docker(not install docker): 该参数表示执行该脚本的过程中不单独安装docker."
			exit 0
		;;
		*)
			echo "无法识别参数$1, 请传入 --help 参数查看提供的选项."
		;;
	esac
	shift 1
done

############ 开始执行 ###############
banner 			# 开始横幅

if [ "Darwin" == "$(uname -s)" ]; then
	install_osx_pkg "$emulator" || exit 1
else
	# Here we will use package managers to determine which operating system the user is using.

	# Suse and derivatives
	if hash 2>/dev/null zypper; then
		suse "$emulator" || exit 1
	# Debian or any derivative of it
	elif hash 2>/dev/null apt-get; then
		install_ubuntu_debian_pkg "$defpackman"  || exit 1
	# Fedora
	elif hash 2>/dev/null dnf; then
		fedora "$emulator" || exit 1
	# Gentoo
	elif hash 2>/dev/null emerge; then
		gentoo "$emulator" || exit 1
	# SolusOS
	elif hash 2>/dev/null eopkg; then
		solus "$emulator" || exit 1
	# Arch linux
	elif hash 2>/dev/null pacman; then
		install_archlinux_pkg || exit 1
	# FreeBSD
	elif hash 2>/dev/null pkg; then
		freebsd "$emulator" || exit 1
	# Unsupported platform
	else
    	printf "\e[31;1mFatal error: \e[0;31mUnsupported platform, please open an issue\[0m" || exit 1
	fi
fi

# 安装rust
rustInstall


#  初始化DragonOS的musl交叉编译工具链
initialize_userland_musl_toolchain
install_python_pkg

# 安装dadk
cargo install dadk || exit 1

bashpath=$(cd `dirname $0`; pwd)

# 创建磁盘镜像
bash ${bashpath}/create_hdd_image.sh
# 编译安装GCC交叉编译工具链
bash ${bashpath}/build_gcc_toolchain.sh -cs -kb -kg || (echo "GCC交叉编译工具链安装失败" && exit 1)
# 编译安装musl交叉编译工具链
bash ${bashpath}/install_musl_gcc.sh || (echo "musl交叉编译工具链安装失败" && exit 1)
# 编译安装grub
bash ${bashpath}/grub_auto_install.sh || (echo "grub安装失败" && exit 1)

# 解决kvm权限问题
USR=$USER
sudo adduser $USR kvm
sudo chown $USR /dev/kvm

congratulations
