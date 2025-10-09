:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: introduction/build_system.md

- Translation time: 2025-10-09 14:37:10

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

(_translated_label___build_dragonos_en)=
# Building DragonOS

## 1. Preface

&emsp;&emsp;Regardless of which method you use to compile DragonOS as described later, you must first follow the steps in this section to initialize your development environment.

&emsp;&emsp;Before you begin, you need a computer running Linux or macOS with an X86-64 processor architecture.

&emsp;&emsp;For Linux distributions, it is recommended to use those with relatively new package repositories, such as Ubuntu 22, Debian, or Arch Linux, which can save you a lot of trouble.

### 1.1 Downloading DragonOS Source Code

Using HTTPS clone:

```shell
git clone https://github.com/DragonOS-Community/DragonOS.git
cd DragonOS
# 使用镜像源更新子模块
make update-submodules-by-mirror
```

For convenience in subsequent development, it is recommended to use SSH cloning (please configure your GitHub SSH key first) to avoid cloning failures due to network issues:

Using SSH clone (please configure your GitHub SSH key first):

```shell
# 使用ssh克隆
git clone git@github.com:DragonOS-Community/DragonOS.git
cd DragonOS
# 使用镜像源更新子模块
make update-submodules-by-mirror
```

## 2. Installation Using One-Click Initialization Script (Recommended)

&emsp;&emsp;We provide a one-click initialization script that can install everything with a single command. Simply run the following command in the console:

```shell
cd DragonOS
cd tools
bash bootstrap.sh  # 这里请不要加上sudo, 因为需要安装的开发依赖包是安装在用户环境而非全局环境
```

:::{note}
The one-click configuration script currently only supports the following systems:

- Ubuntu/Debian/Deepin/UOS and other Debian-based derivatives
- Gentoo: Due to the characteristics of the Gentoo system, when USE flags or circular dependency issues arise, please handle them according to the emerge prompts. For official dependency handling examples, refer to [GentooWiki](https://wiki.gentoo.org/wiki/Handbook:AMD64/Full/Working/zh-cn#.E5.BD.93_Portage_.E6.8A.A5.E9.94.99.E7.9A.84.E6.97.B6.E5.80.99)

You are welcome to improve the build scripts for other systems!
:::

**If the one-click initialization script runs successfully and outputs the final "Congratulations" screen (as shown below), please close the current terminal and reopen it.**

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

**Then, please proceed directly to {ref}`编译命令讲解 <_build_system_command>` for further reading.**

## 3. Manual Installation

### 3.1 Dependency List

&emsp;&emsp;If the automatic installation script does not support your operating system, you will need to manually install the dependencies. The following is the list of dependencies:

&emsp;&emsp;Among the following dependencies, except for `docker-ce` and `Rust及其工具链`, the others can be installed via the system's package manager. For Docker and Rust installation, please refer to the following sections.

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

**Please note that if your Linux system is running in a virtual machine, you must enable the Intel VT-x or AMD-V option in the processor settings tab of your VMware/VirtualBox virtual machine; otherwise, DragonOS will not run.**

:::{note}

*The QEMU built from some Linux distribution repositories may be incompatible with DragonOS due to its low version. If you encounter this issue, please uninstall QEMU and reinstall it by compiling from source.*

Download the QEMU source code from this address: https://download.qemu.org/

After extracting, enter the source code directory and execute the following commands:

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
Please note that the compiled QEMU will connect via VNC mode, so you will also need to install a VNC viewer on your computer to connect to the QEMU virtual machine.
:::

### 3.2 Installing Docker

&emsp;&emsp;You can download and install docker-ce from the Docker official website.

> For details, please go to: [https://docs.docker.com/engine/install/](https://docs.docker.com/engine/install/)

### 3.3 Installing Rust

:::{warning}
**[Common Misconception]**: If you plan to compile using Docker, although the Docker image already has the Rust compilation environment installed, your host machine still needs to have the Rust environment installed to enable Rust-Analyzer code suggestions in VSCode and for the `make clean` command to work properly.
:::

&emsp;&emsp;You can install Rust by entering the following command in the console.

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

**At this point, the common dependencies have been installed. You can proceed to the subsequent sections based on your needs.**

**For the usage of compilation commands, please refer to: {ref}`编译命令讲解 <_build_system_command>`**

## 4. Building from Docker (Not Recommended)

&emsp;&emsp;DragonOS provides a Docker compilation environment to facilitate developers in running DragonOS. However, since the coding process still needs to be done on the host machine, you need to install the Rust compilation environment on the host machine.

&emsp;&emsp;This section assumes all operations are performed under Linux.

### 4.1 Installing QEMU Virtual Machine

&emsp;&emsp;In this section, we recommend installing QEMU via the command line:

```shell
sudo apt install -y qemu qemu-system qemu-kvm
```

### 4.2 Creating a Disk Image

&emsp;&emsp;First, you need to create a virtual disk image using the create_hdd_image.sh script in the tools folder. You need to run this command in the tools folder.

```shell
bash create_hdd_image.sh
```

### 4.3 Running DragonOS

&emsp;&emsp;If all goes well, this will be the final step to run DragonOS. You only need to execute the following command in the root directory of DragonOS to run DragonOS.

```shell
make run-docker
```

&emsp;&emsp;After a short wait, DragonOS will start running.

&emsp;&emsp;After the QEMU virtual machine is launched, you need to enter the letter `c` in the console and press Enter. This will start the virtual machine.

:::{note}
1. During the first compilation, it may take some time to download Rust-related indexes (several hundred MB in size), so please be patient!
2. The command may require sudo privileges.
:::

**For the usage of compilation commands, please refer to: {ref}`编译命令讲解 <_build_system_command>`**

## 5. Other Notes

### 5.1 Creating a Disk Image

&emsp;&emsp;First, you need to run `tools/create_hdd_image.sh` with **regular user** privileges to create a disk image file for DragonOS. This script will automatically complete the creation of the disk image and move it to the `bin/` directory.

&emsp;&emsp;Please note that due to permission issues, you must run this script with **regular user** privileges. (When elevated privileges are required after running, the system may ask you for a password.)

### 5.2 Compiling and Running DragonOS

1. Install the compilation and runtime environment
2. Enter the DragonOS folder
3. Enter `make run` to compile, write to the disk image, and run

&emsp;&emsp;After the QEMU virtual machine is launched, you need to enter the letter `c` in the console and press Enter. This will start the virtual machine.

:::{note}
During the first compilation, it may take some time to download Rust-related indexes (several hundred MB in size), so please be patient!
:::

**For the usage of compilation commands, please refer to: {ref}`编译命令讲解 <_build_system_command>`**

(_translated_label___build_system_command_en)=
## 6. Explanation of Compilation Commands

- Local compilation, no run: `make all -j 您的CPU核心数`
- Local compilation, write to disk image, no run: `make build`
- Local compilation, write to disk image, and run in QEMU: `make run`
- Local compilation, write to disk image, run in headless mode: 
`make run-nographic`
- Docker compilation, write to disk image: `make docker`
- Docker compilation, write to disk image, and run in QEMU: `make run-docker`
- No compilation, directly boot from existing disk image: `make qemu`
- No compilation, directly boot from existing disk image (headless mode): `make qemu-nographic`
- Clean up compilation-generated files: `make clean`
- Compile documentation: `make docs` (requires manual installation of sphinx and dependencies in `requirements.txt` under docs)
- Clean up documentation: `make clean-docs`
- Format code: `make fmt`
- Run and execute syscall tests: `make test-syscall`

:::{note}
If you want to run DragonOS in VNC, add the `-vnc` suffix to the above commands. For example: `make run-vnc`

The QEMU virtual machine will listen for VNC connections on port 5900. You can use a VNC viewer or Remmina to connect to the QEMU virtual machine.
:::

## 7. Compiling for riscv64

Since DragonOS has not yet been fully ported to riscv64, the compilation process requires the following steps:

1. Modify `env.mk` and `.vscode/settings.json`

Change the value of `ARCH` in `env.mk` to `riscv64`, and comment out `"rust-analyzer.cargo.target": "x86_64-unknown-none",` in `setting.json`, replacing it with the line that enables riscv64.

2. Restart rust-analyzer

3. Clean up compilation cache

Due to architectural differences between x86_64 and riscv64, there may be compilation issues caused by caching. Ensure the cache is cleared before running.

```shell
make clean
```

4. Compile and run for riscv64

```shell
# 下载DragonStub
git submodule update --init --recursive --force

make run
```

Please note that since QEMU runs in the console, to exit, enter `Ctrl+A` and press `X`.
