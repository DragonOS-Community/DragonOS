# 构建DragonOS

## 软件依赖

- GNU make
- GCC >= 8.3.0
- xorriso
- grub 2.06

## 开发环境

​    目前，DragonOS在Deepin V20上进行开发。经测试，在Debian bullseye上，可以正常编译、运行。建议使用Docker运行debian镜像进行开发。（后期将会发布开发环境的docker镜像）

## 运行环境

1. qemu 6.2.0（编译安装并启用gdb调试选项）
2. gdb
3. VNC Viewer

## 编译DragonOS

1. 安装编译及运行环境
2. 进入DragonOS文件夹
3. 输入命令：`make -j 16`即可编译

## 运行DragonOS

### 安装软件依赖

​    在运行DragonOS之前，需要先安装需要先安装上述软件依赖。

### 创建磁盘镜像

#### 概述

    使用tools目录下的脚本，创建一至少为16MB磁盘镜像（类型选择raw）。并建立MBR分区表，然后将第一个分区格式化为FAT32分区。

​    在完成以上操作后，将创建的磁盘文件移动至bin文件夹（若不存在，则需要您手动创建），并将其重命名为“disk.img”

​    最后，在DragonOS目录下运行 `bash run.sh`脚本，将会完成编译、文件拷贝、内核镜像打包、启动qemu虚拟机的全过程。当qemu虚拟机启动后，即可使用VNC Viewer连接到虚拟机。

#### 具体操作方法

    首先，您需要使用`tools/create_hdd_image.sh`创建一块磁盘镜像文件，该脚本在创建磁盘镜像之后，会自动调用fdisk，您需要在fdisk之中对虚拟磁盘进行初始化。您需要使用fdisk把磁盘的分区表设置为MBR格式，并创建1个分区。具体操作为：分别输入命令`o`,`n`,`w`。完成操作后，磁盘镜像`disk.img`将会被创建。

    随后，您需要将这个`disk.img`磁盘文件移动到bin/文件夹（需要您手动创建）下。  
并在bin文件夹下创建子文件夹disk_mount。

    接着，使用`tools/mount_virt_disk.sh`，挂载该磁盘镜像到disk_mount文件夹。然后在disk_mount文件夹中，创建子文件夹dev，并在dev文件夹中创建键盘文件`keyboard.dev`  
    至此，准备工作已经完成，您可以运行`run.sh`，然后DragonOS将会被启动。   