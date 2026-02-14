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
CI_INSTALL="false"
APT_FLAG=""
INSTALL_MODE=""

export RUSTUP_DIST_SERVER=${RUSTUP_DIST_SERVER:-https://rsproxy.cn}
export RUSTUP_UPDATE_ROOT=${RUSTUP_UPDATE_ROOT:-https://rsproxy.cn/rustup}
export RUST_VERSION="${RUST_VERSION:-nightly-2025-08-10}"
export NIX_MIRROR=${NIX_MIRROR:-}
export NIX_INSTALLER_URL=${NIX_INSTALLER_URL:-https://mirrors.tuna.tsinghua.edu.cn/nix/latest/install}
export NIX_INSTALLER_FALLBACK_URL=${NIX_INSTALLER_FALLBACK_URL:-https://nixos.org/nix/install}
export NIX_INSTALLER_ARGS=${NIX_INSTALLER_ARGS:---daemon}
export NIX_TRUSTED_USER=${NIX_TRUSTED_USER:-}
export NIX_AUTO_GC=${NIX_AUTO_GC:-}
export NIX_MIN_FREE=${NIX_MIN_FREE:-5G}
export NIX_MAX_FREE=${NIX_MAX_FREE:-10G}

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

congratulations_nix()
{
	echo "|-----------Congratulations!---------------|"
	echo "|                                          |"
	echo "|   Nix 已成功安装!                        |"
	echo "|                                          |"
	echo "|   请[关闭]当前终端, 并[重新打开]一个终端 |"
	echo "|   然后通过以下命令进入开发环境:          |"
	echo "|                                          |"
	echo "|   nix develop                            |"
	echo "|                                          |"
	echo "|------------------------------------------|"
}

####################################################################################
# 配置 Nix 国内镜像源 (可选)
####################################################################################
setup_nix_mirror()
{
	if [ "$CI_INSTALL" = "true" ]; then
		return 0
	fi

	local enable_mirror=""
	if [ -n "$NIX_MIRROR" ]; then
		if echo "$NIX_MIRROR" | grep -Eiq "^(0|false|no)$"; then
			enable_mirror=""
		else
			enable_mirror="true"
		fi
	else
		echo ""
		echo "是否启用国内 Nix 镜像源（清华/中科大）？"
		echo "若你在国内且没有全局代理，强烈推荐开启。"
		printf "(y/N): "
		read mirror_choice
		if echo "$mirror_choice" | grep -iq "^y" ;then
			enable_mirror="true"
		fi
	fi

	if [ "$enable_mirror" = "true" ]; then
		local config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/nix"
		local config_file="${config_dir}/nix.conf"
		mkdir -p "$config_dir"

		if ! grep -q "DragonOS Nix mirror" "$config_file" 2>/dev/null; then
			cat >> "$config_file" <<'EOF'
# DragonOS Nix mirror (CN)
substituters = https://mirrors.tuna.tsinghua.edu.cn/nix-channels/store https://mirrors.ustc.edu.cn/nix-channels/store https://cache.nixos.org/
trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=
EOF
			echo "已写入 Nix 镜像配置: $config_file"
		else
			echo "已检测到 Nix 镜像配置，跳过写入。"
		fi
	fi
}

####################################################################################
# 启用 Nix 实验特性（nix-command + flakes）
####################################################################################
configure_nix_features()
{
	local config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/nix"
	local config_file="${config_dir}/nix.conf"
	mkdir -p "$config_dir"

	if ! grep -q "experimental-features" "$config_file" 2>/dev/null; then
		cat >> "$config_file" <<'EOF'
# DragonOS Nix features
experimental-features = nix-command flakes
EOF
		echo "已写入 Nix 实验特性配置: $config_file"
	else
		echo "已检测到 Nix 实验特性配置，跳过写入。"
	fi
}

####################################################################################
# 将当前用户加入 Nix trusted-users（需要 sudo）
####################################################################################
configure_nix_trusted_user()
{
	if [ "$CI_INSTALL" = "true" ]; then
		return 0
	fi

	local enable_trust=""
	if [ -n "$NIX_TRUSTED_USER" ]; then
		if echo "$NIX_TRUSTED_USER" | grep -Eiq "^(0|false|no)$"; then
			enable_trust=""
		else
			enable_trust="true"
		fi
	else
		echo ""
		echo "是否将当前用户加入 Nix trusted-users？"
		echo "否则会忽略 extra-substituters 等受限配置。"
		printf "(y/N): "
		read trust_choice
		if echo "$trust_choice" | grep -iq "^y" ;then
			enable_trust="true"
		fi
	fi

	if [ "$enable_trust" = "true" ]; then
		local sys_conf="/etc/nix/nix.conf"
		if ! grep -q "DragonOS Nix trusted users" "$sys_conf" 2>/dev/null; then
			sudo sh -c "printf '%s\n' '# DragonOS Nix trusted users' 'trusted-users = root $USER' >> \"$sys_conf\""
			echo "已写入 trusted-users: $sys_conf"
		else
			echo "已检测到 trusted-users 配置，跳过写入。"
		fi
		# 尝试重启 nix-daemon（若尚未安装则忽略失败）
		if command -v systemctl >/dev/null 2>&1; then
			sudo systemctl restart nix-daemon 2>/dev/null || true
		fi
	fi
}

####################################################################################
# 配置 Nix 自动 GC（磁盘空间低于阈值时自动清理）
####################################################################################
configure_nix_auto_gc()
{
	if [ "$CI_INSTALL" = "true" ]; then
		return 0
	fi

	local enable_auto_gc=""
	if [ -n "$NIX_AUTO_GC" ]; then
		if echo "$NIX_AUTO_GC" | grep -Eiq "^(0|false|no)$"; then
			enable_auto_gc=""
		else
			enable_auto_gc="true"
		fi
	else
		echo ""
		echo "是否启用 Nix 自动 GC？（磁盘空间不足时自动清理旧构建）"
		echo "默认阈值: min-free=${NIX_MIN_FREE}, max-free=${NIX_MAX_FREE}"
		printf "(y/N): "
		read auto_gc_choice
		if echo "$auto_gc_choice" | grep -iq "^y" ;then
			enable_auto_gc="true"
		fi
	fi

	if [ "$enable_auto_gc" = "true" ]; then
		local min_free="${NIX_MIN_FREE}"
		local max_free="${NIX_MAX_FREE}"
		# 将 GiB/MiB 形式转换为 Nix 可接受的 G/M
		case "$min_free" in
			*GiB) min_free="${min_free%GiB}G" ;;
			*MiB) min_free="${min_free%MiB}M" ;;
		esac
		case "$max_free" in
			*GiB) max_free="${max_free%GiB}G" ;;
			*MiB) max_free="${max_free%MiB}M" ;;
		esac

		local sys_conf="/etc/nix/nix.conf"
		local user_conf="${XDG_CONFIG_HOME:-$HOME/.config}/nix/nix.conf"
		local target_conf=""

		if [ -w "$sys_conf" ] || sudo -n true 2>/dev/null; then
			target_conf="$sys_conf"
			sudo sh -c "mkdir -p \"$(dirname "$sys_conf")\""
			if sudo sh -c "grep -q 'DragonOS Nix auto gc' \"$sys_conf\" 2>/dev/null"; then
				sudo sed -i "s/^min-free.*/min-free = ${min_free}/" "$sys_conf"
				sudo sed -i "s/^max-free.*/max-free = ${max_free}/" "$sys_conf"
				echo "已更新 Nix 自动 GC 配置: $sys_conf"
			else
				sudo sh -c "printf '%s\n' '# DragonOS Nix auto gc' 'min-free = $min_free' 'max-free = $max_free' >> \"$sys_conf\""
				echo "已写入 Nix 自动 GC 配置: $sys_conf"
			fi
		else
			mkdir -p "$(dirname "$user_conf")"
			target_conf="$user_conf"
			if grep -q "DragonOS Nix auto gc" "$user_conf" 2>/dev/null; then
				sed -i "s/^min-free.*/min-free = ${min_free}/" "$user_conf"
				sed -i "s/^max-free.*/max-free = ${max_free}/" "$user_conf"
				echo "已更新 Nix 自动 GC 配置: $user_conf"
			else
				cat >> "$user_conf" <<EOF
# DragonOS Nix auto gc
min-free = $min_free
max-free = $max_free
EOF
				echo "已写入 Nix 自动 GC 配置: $user_conf"
			fi
		fi
	fi
}

####################################################################################
# 安装 Nix 包管理器（含镜像配置、trusted-users、实验特性、自动 GC 及完成提示）
####################################################################################
install_nix()
{
	# 配置 Nix 镜像
	setup_nix_mirror

	if command -v nix >/dev/null 2>&1; then
		echo "Nix 已经安装在您的系统上。"
	else
		echo "正在安装 Nix 包管理器..."
		set -o pipefail
		if ! curl -fsSL "$NIX_INSTALLER_URL" | sh -s -- $NIX_INSTALLER_ARGS; then
			echo "镜像下载失败，尝试官方地址..."
			if ! curl -fsSL "$NIX_INSTALLER_FALLBACK_URL" | sh -s -- $NIX_INSTALLER_ARGS; then
				echo "Nix 安装脚本下载失败！"
				set +o pipefail
				exit 1
			fi
		fi
		set +o pipefail

		echo "Nix 安装成功！"

		# Source nix environment
		if [ -f "/nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh" ]; then
			. /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh
		fi
	fi

	# 配置 trusted-users
	configure_nix_trusted_user

	# 配置 Nix 实验特性
	configure_nix_features

	# 配置 Nix 自动 GC
	configure_nix_auto_gc

	congratulations_nix
}

####################################################################################
# 询问用户选择安装模式
####################################################################################
ask_install_mode()
{
	if [ "$CI_INSTALL" = "true" ]; then
		INSTALL_MODE="legacy"
		return
	fi

	if [ -n "$INSTALL_MODE" ]; then
		return
	fi

	echo ""
	echo "请选择安装模式:"
	echo "  1) nix    - 仅安装 Nix，使用 nix develop 进入开发环境 (推荐)"
	echo "  2) legacy - 不安装 Nix，仅安装传统依赖 (完整安装)"
	echo ""
	printf "请输入选项 (1/2) [默认: 1]: "
	read mode_choice

	case "$mode_choice" in
		2|legacy)
			INSTALL_MODE="legacy"
			echo "已选择: legacy 模式 (完整安装)"
			;;
		*)
			INSTALL_MODE="nix"
			echo "已选择: nix 模式 (仅 Nix)"
			;;
	esac
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

    sudo "$1" install ${APT_FLAG} -y \
        ca-certificates \
        curl wget \
        unzip \
        gnupg \
        lsb-release \
        llvm-dev libclang-dev clang gcc-multilib \
        gcc build-essential fdisk dosfstools dnsmasq bridge-utils iptables libssl-dev pkg-config \
		python3-sphinx make git meson ninja-build
	# 必须分开安装，否则会出现错误
	sudo "$1" install ${APT_FLAG} -y \
		gcc-riscv64-unknown-elf gcc-riscv64-linux-gnu linux-libc-dev-riscv64-cross gdb-multiarch

	# 如果python3没有安装
	if [ -z "$(which python3)" ]; then
		echo "正在安装python3..."
		sudo apt install ${APT_FLAG} -y python3 python3-pip
	fi

    if [ -z "$(which docker)" ] && [ "${dockerInstall}" = "true" ]; then
        echo "正在安装docker..."
        sudo apt install ${APT_FLAG} -y docker.io docker-compose
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
        sudo $1 install ${APT_FLAG} -y qemu-system qemu-kvm
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
    sudo "${pkgman}"  net-misc/curl net-misc/wget net-misc/bridge-utils net-dns/dnsmasq sys-apps/diffutils dev-util/pkgconf sys-apps/which app-arch/unzip sys-apps/util-linux sys-fs/dosfstools sys-devel/gcc dev-build/make sys-devel/flex sys-apps/texinfo dev-libs/gmp dev-libs/mpfr app-emulation/qemu dev-libs/mpc dev-libs/openssl dev-util/meson dev-util/ninja
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
        libmpc openssl meson ninja

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

	echo "正在安装 Meson 和 Ninja..."
	sudo dnf install -y meson ninja-build
}

install_osx_pkg()
{
    echo "Detected OSX! 暂不支持Mac OSX的一键安装！"
    exit 1
}

freebsd()
{
    echo "Checking QEMU and Meson installation on FreeBSD..."

    # 检查并安装 Meson 和 Ninja
    if ! pkg info -q meson; then
        echo "Meson is not installed. Installing via pkg..."
        sudo pkg update && sudo pkg install -y meson ninja-build
    fi

    # 检查 QEMU 是否已安装
    if pkg info -q qemu; then
        echo "✓ QEMU is already installed."
        echo "QEMU version: $(qemu-system-x86_64 --version | head -n 1)"
        return 0
    else
        echo "QEMU is not installed. Installing via pkg..."

        # 更新包数据库
        if ! sudo pkg update; then
            echo "✗ Failed to update package database" >&2
            return 1
        fi

        # 安装 QEMU
        if sudo pkg install -y qemu; then
            echo "✓ QEMU installed successfully."
            echo "QEMU version: $(qemu-system-x86_64 --version | head -n 1)"

            # 可选：将当前用户添加到kvm组以获得更好的性能
            if pw groupshow kvm >/dev/null 2>&1; then
                echo "Adding user to kvm group for better performance..."
                sudo pw usermod $(whoami) -G kvm
                echo "You may need to logout and login again for group changes to take effect."
            fi

            return 0
        else
            echo "✗ Failed to install QEMU" >&2
            return 1
        fi
    fi
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
		if [ "$CI_INSTALL" = "true" ]; then
		    echo "In CI, skip source change"
		else
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
		fi
		if [ "$change_rust_src" = "true" ]; then
			echo "正在为rust换源"
			bash change_rust_src.sh --sparse
		fi

        echo "正在安装DragonOS所需的rust组件...首次安装需要一些时间来更新索引，请耐心等待..."

		rustup toolchain install $RUST_VERSION-x86_64-unknown-linux-gnu
		rustup component add rust-src --toolchain $RUST_VERSION-x86_64-unknown-linux-gnu
		rustup target add x86_64-unknown-none --toolchain $RUST_VERSION-x86_64-unknown-linux-gnu
		rustup target add x86_64-unknown-linux-musl --toolchain $RUST_VERSION-x86_64-unknown-linux-gnu
		rustup target add riscv64gc-unknown-none-elf --toolchain $RUST_VERSION-x86_64-unknown-linux-gnu
		rustup target add riscv64imac-unknown-none-elf --toolchain $RUST_VERSION-x86_64-unknown-linux-gnu
		rustup target add riscv64gc-unknown-linux-musl --toolchain $RUST_VERSION-x86_64-unknown-linux-gnu
		rustup target add loongarch64-unknown-none --toolchain $RUST_VERSION-x86_64-unknown-linux-gnu

		rustup component add rust-src --toolchain nightly-x86_64-unknown-linux-gnu
		rustup component add rust-src
        rustup component add llvm-tools-preview
		rustup default $RUST_VERSION
		cargo install cargo-binutils
		cargo install bpf-linker

		echo "Rust已经成功的在您的计算机上安装！请运行 source ~/.cargo/env 或 . ~/cargo/env 以使rust在当前窗口生效！"
	fi
}

install_python_pkg()
{
	echo "正在安装python依赖项..."
	# 安装文档生成工具
	sh -c "cd ../docs && python3 -m pip install -r requirements.txt"
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
		"--ci")
		    CI_INSTALL="true"
			DEFAULT_INSTALL="true"
			dockerInstall=""
			APT_FLAG="--no-install-recommends"
		;;
		"--nix")
			INSTALL_MODE="nix"
		;;
		"--legacy")
			INSTALL_MODE="legacy"
		;;
		"--help")
			echo "--no-docker(not install docker): 该参数表示执行该脚本的过程中不单独安装docker."
			echo "--nix: 仅安装 Nix，使用 nix develop 进入开发环境."
			echo "--legacy: 不安装 Nix，仅安装传统依赖 (完整安装)."
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

# 询问安装模式
ask_install_mode

# 仅在 nix 模式下安装并配置 Nix；legacy 模式不安装 Nix
if [ "$INSTALL_MODE" = "nix" ]; then
	install_nix
	exit 0
fi

# 以下是 legacy 模式的安装流程（不安装 Nix，仅安装传统依赖）
echo "安装传统依赖..."

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

# 安装dadk
cargo +nightly install --git https://git.mirrors.dragonos.org.cn/DragonOS-Community/DADK.git --tag v0.6.1 --locked || exit 1

bashpath=$(cd `dirname $0`; pwd)

# 编译安装musl交叉编译工具链
$SHELL ${bashpath}/install_cross_gcc.sh || (echo "musl交叉编译工具链安装失败" && exit 1)

install_python_pkg

if [ "$CI_INSTALL" = "true" ]; then
    echo "CI Skip docs, grub deps install"
else

    $SHELL ${bashpath}/grub_auto_install.sh || (echo "grub安装失败" && exit 1)
fi
# 编译安装grub

# 解决kvm权限问题
USR=$USER
sudo groupadd kvm || echo "kvm组已存在"
sudo usermod -aG kvm $USR
sudo chown $USR /dev/kvm

congratulations
