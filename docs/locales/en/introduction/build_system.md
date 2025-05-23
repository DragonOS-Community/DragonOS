:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: introduction/build_system.md

- Translation time: 2025-05-19 01:44:01

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Building DragonOS

## 1. Introduction

&emsp;&emsp;Regardless of which method you use to compile DragonOS in the following sections, you must first follow the steps in this section to initialize your development environment.

&emsp;&emsp;Before you start, you need a computer running Linux or macOS with an X86-64 processor architecture.

&emsp;&emsp;For Linux distributions, it is recommended to use newer distributions such as Ubuntu 22, Debian, or Arch Linux, which can save you a lot of trouble.

### 1.1 Downloading the DragonOS Source Code

Use `https` to clone:

```shell
git clone https://github.com/DragonOS-Community/DragonOS.git
cd DragonOS
# 使用镜像源更新子模块
make update-submodules-by-mirror
```

For convenience in subsequent development, we recommend using `ssh` to clone (please configure your GitHub SSH Key first) to avoid cloning failures due to network issues:

Use `ssh` to clone (please configure your GitHub SSH Key first):

```shell
# 使用ssh克隆
git clone git@github.com:DragonOS-Community/DragonOS.git
cd DragonOS
# 使用镜像源更新子模块
make update-submodules-by-mirror
```

## 2. Installation Using One-Click Initialization Script (Recommended)

&emsp;&emsp;We provide a one-click initialization script that can install everything with a single command. Just run the following command in the terminal:

```shell
cd DragonOS
cd tools
bash bootstrap.sh  # 这里请不要加上sudo, 因为需要安装的开发依赖包是安装在用户环境而非全局环境
```

:::{note}
The one-click configuration script currently supports the following systems:

- Ubuntu/Debian/Deepin/UOS and other derivatives based on Debian
- Gentoo, due to the characteristics of the Gentoo system, when Gentoo encounters USE or circular dependency issues, please handle them according to the emerge prompt information. Official dependency handling examples [GentooWiki](https://wiki.gentoo.org/wiki/Handbook:AMD64/Full/Working/zh-cn#.E5.BD.93_Portage_.E6.8A.A5.E9.94.99.E7.9A.84.E6.97.B6.E5.80.99)

We welcome you to improve the build script for other systems!
:::

**If the one-click initialization script runs normally and outputs the final "Congratulations" interface (as shown below), please close the current terminal and then reopen it.**

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

**Then, please directly jump to {ref}`编译命令讲解 <_build_system_command>` for reading!**

## 3. Manual Installation

### 3.1 Dependency List

&emsp;&emsp;If the automatic installation script does not support your operating system, you need to manually install the required packages. The following is the list of dependencies:

&emsp;&emsp;Among the following dependencies, except for `docker-ce` and `Rust及其工具链`, the rest can be installed using the system's built-in package manager. For the installation of Docker and Rust, please refer to the following sections.

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
- Rust and its toolchain

**Please note that if your Linux system is running in a virtual machine, please make sure to enable the Intel VT-x or AMD-V option in the processor settings of your VMware/Virtual Box virtual machine, otherwise DragonOS will not be able to run.**

:::{note}

*In some Linux distributions, the Qemu built from the software repository may be incompatible with DragonOS due to an outdated version. If you encounter this issue, uninstall Qemu and reinstall it by compiling from source.*

Download the Qemu source code from this address: https://download.qemu.org/

After decompression, enter the source code directory and execute the following command:

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
Please note that the compiled QEMU will be linked via VNC mode, so you also need to install a VNC viewer on your computer to connect to the QEMU virtual machine.
:::

### 3.2 Installing Docker

&emsp;&emsp;You can download and install docker-ce from the Docker official website.

> For detailed information, please visit: [https://docs.docker.com/engine/install/](https://docs.docker.com/engine/install/)

### 3.3 Installing Rust

:::{warning}
**[Common Misconception]**: If you plan to compile using Docker, although the Docker image already includes a Rust compilation environment, to enable code hints in VSCode using Rust-Analyzer and for the `make clean` command to run normally, you still need to install the Rust environment on your client machine.
:::

&emsp;&emsp;You can install Rust by entering the following command in the terminal.

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

**At this point, the public dependencies have been installed. You can proceed to read the subsequent sections according to your needs.**

**For the usage of the compilation command, please refer to: {ref}`编译命令讲解 <_build_system_command>`**

## 4. Building from Docker (Not Recommended)

&emsp;&emsp;DragonOS provides a Docker compilation environment for developers to run DragonOS. However, since the coding process still needs to be performed on the client machine, you need to install the Rust compilation environment on your client machine.

&emsp;&emsp;This section assumes that all operations are performed under Linux.

### 4.1 Installing QEMU Virtual Machine

&emsp;&emsp;In this section, we recommend installing QEMU via the command line:

```shell
sudo apt install -y qemu qemu-system qemu-kvm
```

### 4.2 Creating a Disk Image

&emsp;&emsp;First, you need to use the `create_hdd_image.sh` script in the `tools` folder to create a virtual disk image. You need to run this command in the `tools` folder.

```shell
bash create_hdd_image.sh
```

### 4.3 Running DragonOS

&emsp;&emsp;If everything goes well, this will be the final step to run DragonOS. You just need to execute the following command in the DragonOS root directory to run DragonOS.

```shell
make run-docker
```

&emsp;&emsp;Wait a moment, DragonOS will be started.

&emsp;&emsp;After the QEMU virtual machine is started, you need to input the letter `c` in the console and press Enter. This will start the virtual machine.

:::{note}
1. During the first compilation, since it requires downloading Rust-related indexes (hundreds of MB in size), it will take some time. Please be patient!
2. Entering commands may require adding `sudo`
:::

**For the usage of the compilation command, please refer to: {ref}`编译命令讲解 <_build_system_command>`**

## 5. Other Notes

### 5.1 Creating a Disk Image

&emsp;&emsp;First, you need to run `tools/create_hdd_image.sh` with **normal user** permissions to create a disk image file for DragonOS. This script will automatically complete the creation of the disk image and move it to the `bin/` directory.

&emsp;&emsp;Please note that due to permission issues, you must run this script with **normal user** permissions. (After running, the system may prompt you to enter a password when you need to elevate permissions.)

### 5.2 Compiling and Running DragonOS

1. Install the compilation and runtime environment
2. Enter the DragonOS folder
3. Input `make run` to compile and write to the disk image, and run

&emsp;&emsp;After the QEMU virtual machine is started, you need to input the letter `c` in the console and press Enter. This will start the virtual machine.

:::{note}
During the first compilation, since it requires downloading Rust-related indexes (hundreds of MB in size), it will take some time. Please be patient!
:::

**For the usage of the compilation command, please refer to: {ref}`编译命令讲解 <_build_system_command>`**

(_translated_label___build_system_command_en)=
## 6. Explanation of Compilation Commands

- Local compilation, no execution: `make all -j 您的CPU核心数`
- Local compilation, write to disk image, no execution: `make build`
- Local compilation, write to disk image, and run in QEMU: `make run`
- Local compilation, write to disk image, and run in headless mode: 
`make run-nographic`
- Docker compilation, write to disk image: `make docker`
- Docker compilation, write to disk image, and run in QEMU: `make run-docker`
- Start directly from an existing disk image without compilation: `make qemu`
- Start directly from an existing disk image without compilation (headless mode): `make qemu-nographic`
- Clean up compiled files: `make clean`
- Compile documentation: `make docs` (requires manual installation of sphinx and dependencies in `requirements.txt`)
- Clean up documentation: `make clean-docs`
- Format code: `make fmt`

:::{note}
If you need to run DragonOS in VNC, add the `-vnc` suffix to the above command. For example: `make run-vnc`

The QEMU virtual machine will listen on port 5900 for VNC connections. You can connect to the QEMU virtual machine using a VNC viewer or Remmina.
:::

## 7. Compiling for riscv64

Since DragonOS has not been fully ported to riscv64 yet, the compilation needs to be done as follows:

1. Modify `env.mk` and `.vscode/settings.json`

Change the value of `ARCH` in `env.mk` to `riscv64`, and in `setting.json`, comment out `"rust-analyzer.cargo.target": "x86_64-unknown-none",` and change it to the line enabling riscv64.

2. Restart rust-analyzer

3. Clean up the compilation cache

Due to the differences between x86_64 and riscv64 architectures, there may be compilation issues caused by cache. Ensure that you clean up the cache before running.

```shell
make clean
```

4. Compile and run for riscv64

```shell
# 下载DragonStub
git submodule update --init --recursive --force

make run
```

Please note that since you are running QEMU in the console, when you want to exit, input `Ctrl+A` and press `X` to do so.
