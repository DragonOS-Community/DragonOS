(_build_dragonos)=
# 构建DragonOS

## 1.写在前面

&emsp;&emsp;无论您采用后文中的何种方式来编译DragonOS，您必须先按照本小节中的步骤，初始化您的开发环境。

&emsp;&emsp;开始之前，您需要一台运行Linux或MacOS的计算机，并且处理器架构为X86-64.

&emsp;&emsp;对于Linux发行版，建议使用Ubuntu22、Debian、Arch Linux这样的，仓库软件版本较新的发行版，这能为您减少很多麻烦。

### 1.1 下载DragonOS的源代码

使用https克隆：

```shell
git clone https://github.com/DragonOS-Community/DragonOS.git
cd DragonOS
# 使用镜像源更新子模块
make update-submodules-by-mirror
```

为了方便后续的开发，我们建议您使用ssh克隆（请先配置好github的SSH Key），以避免由于网络问题导致的克隆失败：


使用ssh克隆（请先配置好github的SSH Key）：

```shell
# 使用ssh克隆
git clone git@github.com:DragonOS-Community/DragonOS.git
cd DragonOS
# 使用镜像源更新子模块
make update-submodules-by-mirror
```

## 2.使用一键初始化脚本进行安装（推荐）


&emsp;&emsp;我们提供了一键初始化脚本，可以一键安装，只需要在控制台运行以下命令：

```shell
cd DragonOS
cd tools
bash bootstrap.sh  # 这里请不要加上sudo, 因为需要安装的开发依赖包是安装在用户环境而非全局环境
```

:::{note}
一键配置脚本目前只支持以下系统：

- Ubuntu/Debian/Deepin/UOS 等基于Debian的衍生版本
- Gentoo 由于Gentoo系统的特性 当gentoo出现USE或循环依赖问题时 请根据emerge提示信息进行对应的处理 官方的依赖处理实例[GentooWiki](https://wiki.gentoo.org/wiki/Handbook:AMD64/Full/Working/zh-cn#.E5.BD.93_Portage_.E6.8A.A5.E9.94.99.E7.9A.84.E6.97.B6.E5.80.99)

欢迎您为其他的系统完善构建脚本！
:::

**如果一键初始化脚本能够正常运行，并输出最终的“祝贺”界面(如下所示)，请关闭当前终端，然后重新打开。**


```shell
|-----------Congratulations!---------------|
|                                          |
|   你成功安装了DragonOS所需的依赖项!          |
|                                          |
|   请关闭当前终端, 并重新打开一个终端          |
|   然后通过以下命令运行:                     |
|                                          |
|                make run                  |
|                                          |
|------------------------------------------|
```

**接着，请直接跳到{ref}`编译命令讲解 <_build_system_command>`进行阅读！**

## 3.手动安装

### 3.1 依赖清单

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
- unzip
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

### 3.2 安装Docker

&emsp;&emsp;您可以在docker官网下载安装docker-ce.

> 详细信息请转到： [https://docs.docker.com/engine/install/](https://docs.docker.com/engine/install/)

### 3.3 安装Rust

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

**至此，公共依赖项已经安装完成，您可以根据自己的需要，阅读后续章节**

**关于编译命令的用法，请见：{ref}`编译命令讲解 <_build_system_command>`**

## 4.从Docker构建（不推荐）

&emsp;&emsp;DragonOS发布了一个Docker编译环境，便于开发者运行DragonOS。但是，由于编码过程仍需要在客户机上进行，因此，您需要在客户机上安装Rust编译环境。

&emsp;&emsp;本节假设以下操作均在Linux下进行。


### 4.1 安装qemu虚拟机

&emsp;&emsp;在本节中，我们建议您采用命令行安装qemu：

```shell
sudo apt install -y qemu qemu-system qemu-kvm
```

### 4.2 创建磁盘镜像

&emsp;&emsp;首先，您需要使用tools文件夹下的create_hdd_image.sh，创建一块虚拟磁盘镜像。您需要在tools文件夹下运行此命令。

```shell
bash create_hdd_image.sh
```

### 4.3 运行DragonOS

&emsp;&emsp;如果不出意外的话，这将是运行DragonOS的最后一步。您只需要在DragonOS的根目录下方，执行以下命令，即可运行DragonOS。

```shell
make run-docker
```

&emsp;&emsp;稍等片刻，DragonOS将会被运行。

&emsp;&emsp;在qemu虚拟机被启动后，我们需要在控制台输入字母`c`，然后回车。这样，虚拟机就会开始执行。

:::{note}
1. 首次编译时，由于需要下载Rust相关的索引（几百MB大小），因此需要一定的时间，请耐心等候！
2. 输入命令可能需要加上sudo
:::

**关于编译命令的用法，请见：{ref}`编译命令讲解 <_build_system_command>`**

## 5.其他注意事项

### 5.1 创建磁盘镜像

&emsp;&emsp;首先，您需要使用**普通用户**权限运行`tools/create_hdd_image.sh`，为DragonOS创建一块磁盘镜像文件。该脚本会自动完成创建磁盘镜像的工作，并将其移动到`bin/`目录下。

&emsp;&emsp;请注意，由于权限问题，请务必使用**普通用户**权限运行此脚本。（运行后，需要提升权限时，系统可能会要求您输入密码）


### 5.2 编译、运行DragonOS

1. 安装编译及运行环境
2. 进入DragonOS文件夹
3. 输入`make run`即可编译并写入磁盘镜像，并运行


&emsp;&emsp;在qemu虚拟机被启动后，我们需要在控制台输入字母`c`，然后回车。这样，虚拟机就会开始执行。

:::{note}
首次编译时，由于需要下载Rust相关的索引（几百MB大小），因此需要一定的时间，请耐心等候！
:::

**关于编译命令的用法，请见：{ref}`编译命令讲解 <_build_system_command>`**

(_build_system_command)=
## 6.编译命令讲解

- 本地编译，不运行: `make all -j 您的CPU核心数`
- 本地编译，并写入磁盘镜像，不运行: `make build`
- 本地编译，写入磁盘镜像，并在QEMU中运行: `make run`
- 本地编译，写入磁盘镜像，以无图形模式运行: 
`make run-nographic`
- Docker编译，并写入磁盘镜像,: `make docker`
- Docker编译，写入磁盘镜像，并在QEMU中运行: `make run-docker`
- 不编译，直接从已有的磁盘镜像启动: `make qemu`
- 不编译，直接从已有的磁盘镜像启动（无图形模式）: `make qemu-nographic`
- 清理编译产生的文件: `make clean`
- 编译文档: `make docs` （需要手动安装sphinx以及docs下的`requirements.txt`中的依赖）
- 清理文档: `make clean-docs`
- 格式化代码: `make fmt`

:::{note}
如果您需要在vnc中运行DragonOS，请在上述命令后加上`-vnc`后缀。如：`make run-vnc`

qemu虚拟机将在5900端口监听vnc连接。您可以使用vnc viewer或者Remmina连接至qemu虚拟机。
:::

## 7. 为riscv64编译

由于目前DragonOS尚未完全移植到riscv64，因此编译需要这样做：

1. 修改`env.mk`和`.vscode/settings.json`

把`env.mk`里面的`ARCH`的值改为`riscv64`，并且在`setting.json`里面注释`"rust-analyzer.cargo.target": "x86_64-unknown-none",`，改为启用riscv64的那行。

2. 重启rust-analyzer

3. 清理编译缓存

由于x86_64和riscv64架构差异，可能存在缓存导致的编译问题，确保运行前先清理缓存。

```shell
make clean
```

4. 为riscv64编译并运行

```shell
# 下载DragonStub
git submodule update --init --recursive --force

make run
```

请注意，由于是在控制台运行qemu，当你想要退出的时候，输入`Ctrl+A`然后按`X`即可。
