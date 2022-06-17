# 构建DragonOS

## 软件依赖

- GNU make
- GCC >= 8.3.0
- xorriso
- grub 2.04

## 开发环境

​	目前，DragonOS在Deepin V20上进行开发。经测试，在Debian bullseye上，可以正常编译、运行。建议使用Docker运行debian镜像进行开发。（后期将会发布开发环境的docker镜像）

## 运行环境

1. qemu 6.2.0（编译安装并启用gdb调试选项）
2. gdb
3. VNC Viewer

## 编译DragonOS

1. 安装编译及运行环境
2. 进入DragonOS文件夹
3. 输入命令：`make -j 16`即可编译

## 运行DragonOS

​	在运行DragonOS之前，需要先使用tools目录下的脚本，创建一至少为16MB磁盘镜像（类型选择raw）。并建立MBR分区表，然后将第一个分区格式化为FAT32分区。

​	在完成以上操作后，将创建的磁盘文件移动至bin文件夹（若不存在，则需要您手动创建），并将其重命名为“disk.img”

​	最后，在DragonOS目录下运行 `bash run.sh`脚本，将会完成编译、文件拷贝、内核镜像打包、启动qemu虚拟机的全过程。当qemu虚拟机启动后，即可使用VNC Viewer连接到虚拟机。