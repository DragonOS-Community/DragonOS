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
DEFAULT_INSTALL="false"

export RUSTUP_DIST_SERVER=${RUSTUP_DIST_SERVER:-https://rsproxy.cn}
export RUSTUP_UPDATE_ROOT=${RUSTUP_UPDATE_ROOT:-https://rsproxy.cn/rustup}
export RUST_VERSION="${RUST_VERSION:-nightly-2024-11-05}"
export RUST_VERSION_OLD="${RUST_VERSION:-nightly-2024-07-23}"

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
	echo "|          make run-nographic              |"
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
		python3-sphinx make git
	# 必须分开安装，否则会出现错误
	sudo "$1" install -y \
		gcc-riscv64-unknown-elf gcc-riscv64-linux-gnu gdb-multiarch
	
	# 如果python3没有安装
	if [ -z "$(which python3)" ]; then
		echo "正在安装python3..."
		sudo apt install -y python3 python3-pip
	fi

    if [ -z "$(which docker)" ] && [ "${dockerInstall}" = "true" ]; then
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


####################################
# 当检测到gentoo时，执行此函数         #
####################################
gentoo()
{
    pkgman="emerge"
    echo "检测到Gentoo发行版"
    echo "正在更新包管理器的列表..."
    sudo "${pkgman}" --sync
    echo "正在安装所需的包..."
    sudo "${pkgman}"  net-misc/curl net-misc/wget net-misc/bridge-utils net-dns/dnsmasq sys-apps/diffutils dev-util/pkgconf sys-apps/which app-arch/unzip sys-apps/util-linux sys-fs/dosfstools sys-devel/gcc dev-build/make sys-devel/flex sys-apps/texinfo dev-libs/gmp dev-libs/mpfr app-emulation/qemu dev-libs/mpc dev-libs/openssl
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

install_centos_pkg()
{
	echo "检测到 Centos/Fedora/RHEL 8"
	echo "正在更新包管理器的列表..."
	sudo dnf update -y
	echo "正在安装所需的包"

	echo "正在安装Development Tools..."
	sudo dnf groupinstall -y "Development Tools"

	echo "正在安装LLVM和Clang..."
	sudo dnf install -y llvm-devel clang-devel

	echo "正在安装Clang和GCC..."
	sudo dnf install -y clang gcc-c++

	echo "正在安装QEMU和KVM..."
	sudo dnf install -y qemu qemu-kvm qemu-system-x86

	echo "正在安装fdisk和redhat-lsb-core..."
	sudo dnf install -y util-linux redhat-lsb-core

	echo "正在安装Git..."
	sudo dnf install -y git

	echo "正在安装dosfstools..."
	sudo dnf install -y dosfstools

	echo "正在安装unzip..."
	sudo dnf install -y unzip

	echo "安装bridge utils"
	sudo dnf install -y bridge-utils || sudo rpm -ivh http://mirror.centos.org/centos/7/os/x86_64/Packages/bridge-utils-1.5-9.el7.x86_64.rpm #Centos8 需要直接安装Binary

	echo "安装dnsmasq"
	sudo dnf install -y dnsmasq
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
		echo "正在安装Rust..."
		#install rustup
		curl https://sh.rustup.rs -sSf --retry 5 --retry-delay 5 | sh -s -- --default-toolchain ${RUST_VERSION} -y
		# You have to add the rustup variables to the $PATH
		echo "export PATH=\"\$HOME/.cargo/bin:\$PATH\"" >> ~/.bashrc
		# source the variables so that we can execute rustup commands in the current shell
		source ~/.cargo/env
		source "$HOME/.cargo/env"
	fi
	#
	if [ -z "$(which rustc)" ]; then
		echo "Rust 还未被安装"
		echo "请再次运行脚本，接受rustup安装"
		echo "或通过以下方式手动安装rustc（不推荐）："
		echo "curl https://sh.rustup.rs -sSf | sh -s -- --default-toolchain $RUST_VERSION -y"
		exit
	else
		local change_rust_src=""
		if [ "$DEFAULT_INSTALL" = "true" ]; then
			change_rust_src="true"
		else
			echo "是否为Rust换源为国内镜像源？(Tuna)"
			echo "如果您在国内，我们推荐您这样做，以提升网络速度。"
			echo "*WARNING* 这将会替换原有的镜像源设置。"
			printf "(y/N): "
			read change_src
			if echo "$change_src" | grep -iq "^y" ;then
				change_rust_src="true"
			else
				echo "取消换源，您原有的配置不会被改变。"
			fi
		fi
		if [ "$change_rust_src" = "true" ]; then
			echo "正在为rust换源"
			bash change_rust_src.sh --sparse
		fi

        echo "正在安装DragonOS所需的rust组件...首次安装需要一些时间来更新索引，请耐心等待..."
        
		rustup toolchain install $RUST_VERSION-x86_64-unknown-linux-gnu
		rustup toolchain install $RUST_VERSION_OLD-x86_64-unknown-linux-gnu
		rustup component add rust-src --toolchain $RUST_VERSION-x86_64-unknown-linux-gnu
		rustup component add rust-src --toolchain $RUST_VERSION_OLD-x86_64-unknown-linux-gnu
		rustup target add x86_64-unknown-none --toolchain $RUST_VERSION-x86_64-unknown-linux-gnu
		rustup target add x86_64-unknown-none --toolchain $RUST_VERSION_OLD-x86_64-unknown-linux-gnu
		rustup target add x86_64-unknown-linux-musl --toolchain $RUST_VERSION-x86_64-unknown-linux-gnu
		rustup target add x86_64-unknown-linux-musl --toolchain $RUST_VERSION_OLD-x86_64-unknown-linux-gnu
		rustup target add riscv64gc-unknown-none-elf --toolchain $RUST_VERSION-x86_64-unknown-linux-gnu
		rustup target add riscv64gc-unknown-none-elf --toolchain $RUST_VERSION_OLD-x86_64-unknown-linux-gnu
		rustup target add riscv64imac-unknown-none-elf --toolchain $RUST_VERSION-x86_64-unknown-linux-gnu
		rustup target add riscv64imac-unknown-none-elf --toolchain $RUST_VERSION_OLD-x86_64-unknown-linux-gnu
		rustup target add riscv64gc-unknown-linux-musl --toolchain $RUST_VERSION-x86_64-unknown-linux-gnu
		rustup target add riscv64gc-unknown-linux-musl --toolchain $RUST_VERSION_OLD-x86_64-unknown-linux-gnu
		rustup target add loongarch64-unknown-none --toolchain $RUST_VERSION-x86_64-unknown-linux-gnu
		rustup target add loongarch64-unknown-none --toolchain $RUST_VERSION_OLD-x86_64-unknown-linux-gnu

		rustup component add rust-src --toolchain nightly-x86_64-unknown-linux-gnu
		rustup component add rust-src
        rustup component add llvm-tools-preview
		rustup default $RUST_VERSION
		cargo install cargo-binutils
		cargo install bpf-linker
		
		echo "Rust已经成功的在您的计算机上安装！请运行 source ~/.cargo/env 以使rust在当前窗口生效！"
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
		"--default")
			DEFAULT_INSTALL="true"
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
		install_centos_pkg || exit 1
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

install_python_pkg

# 安装dadk
cargo install dadk || exit 1

bashpath=$(cd `dirname $0`; pwd)

# 编译安装musl交叉编译工具链
$SHELL ${bashpath}/install_cross_gcc.sh || (echo "musl交叉编译工具链安装失败" && exit 1)
# 编译安装grub
$SHELL ${bashpath}/grub_auto_install.sh || (echo "grub安装失败" && exit 1)

# 解决kvm权限问题
USR=$USER
sudo groupadd kvm || echo "kvm组已存在"
sudo usermod -aG kvm $USR
sudo chown $USR /dev/kvm

congratulations
