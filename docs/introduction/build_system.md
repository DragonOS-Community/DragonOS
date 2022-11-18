# 构建DragonOS

## 从Docker构建（推荐）

&emsp;&emsp;为减轻配置环境的负担，DragonOS发布了一个Docker编译环境，便于开发者运行DragonOS。我们强烈建议您采用这种方式来运行DragonOS。

&emsp;&emsp;本节假设以下操作均在Linux下进行。

### 安装Docker

&emsp;&emsp;您可以在docker官网下载安装docker-ce.

> 详细信息请转到： https://docs.docker.com/engine/install/

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

&emsp;&emsp;请留意，若您的Linux系统是在虚拟机中运行的，还请您在您的VMware/Virtual Box虚拟机的处理器设置选项卡中，开启Intel VT-x或AMD-V选项，否则，DragonOS将无法运行。

*在某些Linux发行版的软件仓库中构建的Qemu可能存在不识别命令参数的问题，如果遇到这种问题，请卸载Qemu，并采用编译安装的方式重新安装Qemu*

在该地址下载Qemu源代码： https://download.qemu.org/

解压后进入源代码目录，然后执行下列命令：

```shell
./configure --enable-kvm
make -j 8
sudo make install
```

### 下载DragonOS的源代码

&emsp;&emsp;假设您的计算机上已经安装了git，您可以通过以下命令，获得DragonOS的最新的源代码：

```shell
git clone https://github.com/fslongjin/DragonOS
cd DragonOS
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
首次编译时，由于需要下载Rust相关的索引（几百MB大小），因此需要一定的时间，请耐心等候！
:::

**关于编译命令的用法，请见：{ref}`编译命令讲解 <_build_system_command>`**

## 手动搭建开发环境

&emsp;&emsp;若您追求快速的编译速度，以及完整的开发调试支持，且愿意花费半个小时到两个小时的时间来配置开发环境的话，该小节的内容能帮助到您。

### 软件依赖

&emsp;&emsp;您需要编译安装以下软件依赖。他们的源代码可以在对应项目的官方网站上获得。

- grub 2.06 (不必使用sudo权限进行install)
- qemu 6.2.0 (启用所有选项)

&emsp;&emsp;需要注意的是，编译安装qemu将会是一件费时费力的工作，它可能需要花费你40分钟以上的时间。

&emsp;&emsp;对于其余的软件依赖，我们提供了一键配置脚本，可以一键安装，只需要在控制台运行以下命令：

```shell
cd tools
bash bootstrap.sh
```
:::{note}
一键配置脚本目前只支持以下系统：

- Ubuntu/Debian/Deepin/UOS 等基于Debian的衍生版本

欢迎您为其他的系统完善构建脚本！
:::


### 创建磁盘镜像

&emsp;&emsp;首先，您需要使用`sudo`权限运行`tools/create_hdd_image.sh`，为DragonOS创建一块磁盘镜像文件。该脚本会自动完成创建磁盘镜像的工作，并将其移动到`bin/`目录下。


### 编译DragonOS

1. 安装编译及运行环境
2. 进入DragonOS文件夹
3. 输入命令：`make -j 16`即可编译
4. 输入`make build`即可编译并写入磁盘镜像




### 运行DragonOS

&emsp;&emsp;至此，准备工作已经完成，您可以在DragonOS项目的根目录下，输入

```shell
make run
```

&emsp;&emsp;然后，DragonOS将会被启动，您可以通过VNC Viewer连接至虚拟机。在qemu虚拟机被启动后，我们需要在控制台输入字母`c`，然后回车。这样，虚拟机就会开始执行。

:::{note}
首次编译时，由于需要下载Rust相关的索引（几百MB大小），因此需要一定的时间，请耐心等候！
:::

**关于编译命令的用法，请见：{ref}`编译命令讲解 <_build_system_command>`**

(_build_system_command)=
## 编译命令讲解

- 本地编译，不运行: `make all -j 您的CPU核心数`
- 本地编译，并写入磁盘镜像，不运行: `make build`
- 本地编译，写入磁盘镜像，并在QEMU中运行: `make run`
- Docker编译，并写入磁盘镜像,: `make docker`
- Docker编译，写入磁盘镜像，并在QEMU中运行: `make run-docker`
- 不编译，直接从已有的磁盘镜像启动: `make qemu`