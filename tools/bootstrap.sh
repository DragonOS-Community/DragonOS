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
    echo "|   请关闭当前终端, 并重新打开一个终端     |"
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
        curl \
        gnupg \
        lsb-release \
        llvm-dev libclang-dev clang gcc-multilib \
        gcc build-essential fdisk dosfstools dnsmasq bridge-utils iptables

    if [ -z "$(which docker)" ] && [ -n ${dockerInstall} ]; then
        echo "正在安装docker..."
        sudo mkdir -p /etc/apt/keyrings
        curl -fsSL https://download.docker.com/linux/debian/gpg | sudo gpg --dearmor -o /etc/apt/keyrings/docker.gpg
        echo \
            "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/debian \
            $(lsb_release -cs) stable" | sudo tee /etc/apt/sources.list.d/docker.list > /dev/null
        sudo $1 update
        sudo "$1" install -y docker-ce docker-ce-cli containerd.io docker-compose-plugin
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
		echo "curl -sSf https://static.rust-lang.org/rustup.sh | sh -s -- --channel=nightly"
		exit
	else
        echo "是否为Rust换源为Gitee镜像源？"
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
        rustup toolchain install nightly
        rustup default nightly
        rustup component add rust-src
        rustup component add llvm-tools-preview
		rustup target add x86_64-unknown-none
		echo "Rust已经成功的在您的计算机上安装！请运行 source ~/.cargo/env 以使rust在当前窗口生效！"
	fi
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
rustInstall     # 安装rust

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
		archLinux "$emulator" || exit 1
	# FreeBSD
	elif hash 2>/dev/null pkg; then
		freebsd "$emulator" || exit 1
	# Unsupported platform
	else
    	printf "\e[31;1mFatal error: \e[0;31mUnsupported platform, please open an issue\[0m" || exit 1
	fi
fi

# 创建磁盘镜像
bash create_hdd_image.sh
# 编译安装GCC交叉编译工具链
bash build_gcc_toolchain.sh
# 编译安装grub
bash grub_auto_install.sh

# 解决kvm权限问题
USR=$USER
sudo adduser $USR kvm
sudo chown $USR /dev/kvm

congratulations