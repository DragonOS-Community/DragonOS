(_build_dragonos)=
# 构建DragonOS

## 1.写在前面

&emsp;&emsp;无论您采用后文中的何种方式来编译DragonOS，您必须先按照本小节中的步骤，初始化您的开发环境。

&emsp;&emsp;开始之前，您需要一台运行Linux或MacOS的计算机，并且处理器架构为X86-64.

&emsp;&emsp;对于Linux发行版，建议使用Ubuntu22、Debian、Arch Linux这样的，仓库软件版本较新的发行版，这能为您减少很多麻烦。

### 使用一键初始化脚本进行安装

&emsp;&emsp;我们提供了一键初始化脚本，可以一键安装，只需要在控制台运行以下命令：

```shell
cd tools
bash bootstrap.sh  # 这里请不要加上sudo, 因为需要安装的开发依赖包是安装在用户环境而非全局环境
```

:::{note}
一键配置脚本目前只支持以下系统：

- Ubuntu/Debian/Deepin/UOS 等基于Debian的衍生版本

欢迎您为其他的系统完善构建脚本！
:::

**如果一键初始化脚本能够正常运行，并输出最终的“祝贺”界面(如下所示)，那么恭喜你，可以直接跳到{ref}`这里 <_get_dragonos_source_code>`进行阅读！**

```shell
|-----------Congratulations!---------------|
|                                          |
|   你成功安装了DragonOS所需的依赖项!          |
|   您可以通过以下命令运行它:                  |
|                                          |
|   make run-docker -j 你的cpu核心数         |
|                                          |
|------------------------------------------|
```

### 依赖清单（手动安装）

&emsp;&emsp;如果自动安装脚本不能支持您的操作系统，那么您需要手动安装依赖程序。以下是依赖项的清单：

&emsp;&emsp;在以下依赖项中，除了`docker-ce`和`Rust及其工具链`以外，其他的都能通过系统自带的包管理器进行安装。关于docker以及rust的安装，请看后文。

- docker-ce
- llvm-dev
- libclang-dev
- clang
- gcc-multilib
- qemu qemu-system qemu-kvm
- build-essential
- fdisk
- lsb-release
- git
- dosfstools
- Rust以及其工具链

**请留意，若您的Linux系统是在虚拟机中运行的，还请您在您的VMware/Virtual Box虚拟机的处理器设置选项卡中，开启Intel VT-x或AMD-V选项，否则，DragonOS将无法运行。**

:::{note}


*在某些Linux发行版的软件仓库中构建的Qemu可能由于版本过低而不兼容DragonOS，如果遇到这种问题，请卸载Qemu，并采用编译安装的方式重新安装Qemu*

在该地址下载Qemu源代码： https://download.qemu.org/

解压后进入源代码目录，然后执行下列命令：

```shell
# 安装编译依赖项
sudo apt install -y autoconf automake autotools-dev curl libmpc-dev libmpfr-dev libgmp-dev \
              gawk build-essential bison flex texinfo gperf libtool patchutils bc \
              zlib1g-dev libexpat-dev pkg-config  libglib2.0-dev libpixman-1-dev libsdl2-dev \
              git tmux python3 python3-pip ninja-build

./configure --enable-kvm
make -j 8
sudo make install
# 编译安装完成
```
请注意，编译安装的QEMU，将通过VNC模式进行链接，因此，您还需要在您的计算机上安装VNC viewer以连接至QEMU虚拟机。
:::

### 安装Docker

&emsp;&emsp;您可以在docker官网下载安装docker-ce.

> 详细信息请转到： [https://docs.docker.com/engine/install/](https://docs.docker.com/engine/install/)

### 安装Rust

:::{warning}
**【常见误区】**：如果您打算采用docker进行编译，尽管docker镜像中已经安装了Rust编译环境，但是，为了能够在VSCode中使用Rust-Analyzer进行代码提示，以及`make clean`命令能正常运行，您的客户机上仍然需要安装rust环境。
:::

&emsp;&emsp;您可以在控制台输入以下命令，安装rust。

```shell
# 这两行用于换源，加速Rust的安装过程
export RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static
export RUSTUP_UPDATE_ROOT=https://mirrors.ustc.edu.cn/rust-static/rustup
# 安装Rust
curl https://sh.rustup.rs -sSf | sh -s -- --default-toolchain nightly
# 把Rustup加到环境变量
echo "export PATH=\"\$HOME/.cargo/bin:\$PATH\"" >> ~/.bashrc
source ~/.cargo/env
source "$HOME/.cargo/env"

# 更换cargo的索引源
touch ~/.cargo/config
echo -e "[source.crates-io]   \n \
registry = \"https://github.com/rust-lang/crates.io-index\"  \n \
\n \
replace-with = 'dragonos-gitee' \n \
[source.dragonos-gitee] \n \
registry = \"https://gitee.com/DragonOS/crates.io-index.git\"	 \n \
" > ~/.cargo/config

# 安装DragonOS所需的工具链
cargo install cargo-binutils
rustup toolchain install nightly
rustup default nightly
rustup component add rust-src
rustup component add llvm-tools-preview
rustup target add x86_64-unknown-none
# Rust安装完成
```

### 解决KVM权限问题

&emsp;&emsp;在部分计算机上，可能由于权限问题而无法启动虚拟机，我们可以通过把当前用户加到kvm用户组的方式，解决该问题：

```shell
# 解决kvm权限问题
USR=$USER
sudo adduser $USR kvm
sudo chown $USR /dev/kvm
```

(_get_dragonos_source_code)=
### 下载DragonOS的源代码

&emsp;&emsp;假设您的计算机上已经安装了git，您可以通过以下命令，获得DragonOS的最新的源代码：

```shell
git clone https://github.com/DragonOS-Community/DragonOS
cd DragonOS
```

**至此，公共依赖项已经安装完成，您可以根据自己的需要，阅读后续章节**

## 2.从Docker构建（推荐）

&emsp;&emsp;为减轻配置环境的负担，DragonOS发布了一个Docker编译环境，便于开发者运行DragonOS。我们强烈建议您采用这种方式来运行DragonOS。

&emsp;&emsp;本节假设以下操作均在Linux下进行。

### 获取DragonOS编译镜像

&emsp;&emsp;当您成功安装了docker之后，您可以通过以下命令，下载DragonOS的编译镜像：

```shell
docker pull dragonos/dragonos-dev:v1.1.0-beta3
```

### 安装qemu虚拟机

&emsp;&emsp;在本节中，我们建议您采用命令行安装qemu：

```shell
sudo apt install -y qemu qemu-system qemu-kvm
```

### 创建磁盘镜像

&emsp;&emsp;首先，您需要使用tools文件夹下的create_hdd_image.sh，创建一块虚拟磁盘镜像。您需要在tools文件夹下运行此命令。

```shell
bash create_hdd_image.sh
```

### 运行DragonOS

&emsp;&emsp;如果不出意外的话，这将是运行DragonOS的最后一步。您只需要在DragonOS的根目录下方，执行以下命令，即可运行DragonOS。

```shell
make run-docker
```

&emsp;&emsp;稍等片刻，DragonOS将会被运行。

&emsp;&emsp;在qemu虚拟机被启动后，我们需要在控制台输入字母`c`，然后回车。这样，虚拟机就会开始执行。

:::{note}
(1) 首次编译时，由于需要下载Rust相关的索引（几百MB大小），因此需要一定的时间，请耐心等候！
(2) 输入命令可能需要加上sudo
:::

**关于编译命令的用法，请见：{ref}`编译命令讲解 <_build_system_command>`**

## 3.在本机中直接编译

&emsp;&emsp;若您追求快速的编译速度，以及完整的开发调试支持，且愿意花费半个小时来配置开发环境的话，该小节的内容能帮助到您。

### 软件依赖

&emsp;&emsp;您需要通过以下命令，获取您本机安装的Grub的版本：

```shell
grub-install --version
```

&emsp;&emsp;**如果显示的版本号为2.06及以上，且您已经按照第一小节中的内容，安装相关的依赖，那么，恭喜您，您可以直接在本机编译DragonOS!**

&emsp;&emsp;否则，您需要编译安装Grub-2.06。它的源代码可以通过[https://ftp.gnu.org/gnu/grub/grub-2.06.tar.gz](https://ftp.gnu.org/gnu/grub/grub-2.06.tar.gz)获得。

- grub 2.06 (不必使用sudo权限进行install)

### 创建磁盘镜像

&emsp;&emsp;首先，您需要使用`sudo`权限运行`tools/create_hdd_image.sh`，为DragonOS创建一块磁盘镜像文件。该脚本会自动完成创建磁盘镜像的工作，并将其移动到`bin/`目录下。


### 编译、运行DragonOS

1. 安装编译及运行环境
2. 进入DragonOS文件夹
3. 输入`make run`即可编译并写入磁盘镜像，并运行


&emsp;&emsp;在qemu虚拟机被启动后，我们需要在控制台输入字母`c`，然后回车。这样，虚拟机就会开始执行。

:::{note}
首次编译时，由于需要下载Rust相关的索引（几百MB大小），因此需要一定的时间，请耐心等候！
:::

**关于编译命令的用法，请见：{ref}`编译命令讲解 <_build_system_command>`**

(_build_system_command)=
## 4.编译命令讲解

- 本地编译，不运行: `make all -j 您的CPU核心数`
- 本地编译，并写入磁盘镜像，不运行: `make build`
- 本地编译，写入磁盘镜像，并在QEMU中运行: `make run`
- Docker编译，并写入磁盘镜像,: `make docker`
- Docker编译，写入磁盘镜像，并在QEMU中运行: `make run-docker`
- 不编译，直接从已有的磁盘镜像启动: `make qemu`