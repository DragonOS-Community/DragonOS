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
docker pull dragonos/dragonos-dev:v1.0
```

### 安装qemu虚拟机

&emsp;&emsp;在本节中，我们建议您采用命令行安装qemu：

```shell
sudo apt install -y qemu qemu-system qemu-system-x86_64 qemu-kvm
```

&emsp;&emsp;请留意，若您的Linux系统是在虚拟机中运行的，还请您在您的VMware/Virtual Box虚拟机的处理器设置选项卡中，开启Intel VT-x或AMD-V选项，否则，DragonOS将无法运行。

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
bash run.sh --docker
```

&emsp;&emsp;若输入密码后仍提示权限不足，您可以使用以下命令运行：

```shell
sudo bash run.sh --docker
```

&emsp;&emsp;稍等片刻，DragonOS将会被运行。

&emsp;&emsp;在qemu虚拟机被启动后，我们需要在控制台输入字母`c`，然后回车。这样，虚拟机就会开始执行。



## 手动搭建开发环境

&emsp;&emsp;若您追求快速的编译速度，以及完整的开发调试支持，且愿意花费半个小时到两个小时的时间来配置开发环境的话，该小节的内容能帮助到您。

### 软件依赖

&emsp;&emsp;您需要编译安装以下软件依赖。他们的源代码可以在对应项目的官方网站上获得。

- grub 2.06 (不必使用sudo权限进行install)
- qemu 6.2.0 (启用所有选项)

&emsp;&emsp;需要注意的是，编译安装qemu将会是一件费时费力的工作，它可能需要花费你40分钟以上的时间。

&emsp;&emsp;对于以下软件依赖，建议您使用系统自带的包管理器进行安装。

- gcc >= 8.3.0

- xorriso

- fdisk

- make

- VNC Viewer

- gdb



### 编译DragonOS

1. 安装编译及运行环境
2. 进入DragonOS文件夹
3. 输入命令：`make -j 16`即可编译



### 创建磁盘镜像

&emsp;&emsp;首先，您需要使用`sudo`权限运行`tools/create_hdd_image.sh`，为DragonOS创建一块磁盘镜像文件。该脚本会自动完成创建磁盘镜像的工作，并将其移动到`bin/`目录下。

### 运行DragonOS

&emsp;&emsp;至此，准备工作已经完成，您可以在DragonOS项目的根目录下，输入

```shell
bash run.sh
```

&emsp;&emsp;然后，DragonOS将会被启动，您可以通过VNC Viewer连接至虚拟机。在qemu虚拟机被启动后，我们需要在控制台输入字母`c`，然后回车。这样，虚拟机就会开始执行。
