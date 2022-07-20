# DragonOS

**Languages** 中文|[English](README_EN.md)

&nbsp;

这是一个运行于x86_64平台的64位操作系统。目前正在开发之中！

## 网站
- 项目官网  **[DragonOS.org](https://dragonos.org)**
- 项目文档  **[docs.DragonOS.org](https://docs.dragonos.org)**
- 开源论坛  **[bbs.DragonOS.org](https://bbs.dragonos.org)**
- 开发交流QQ群 **115763565**
&nbsp;
## 开发环境

GCC>=8.0

qemu==6.2

grub==2.06

## 如何运行？

1. clone本项目

2. 运行命令 bash run.sh

## To do list:

- [x] multiboot2

- [x] printk

- [x] 简单的异常捕获及中断处理

- [x] APIC

- [x] 初级内存管理单元

- [x] SLAB内存池

- [x] PS/2 键盘、鼠标驱动

- [x] PCI 总线驱动

- [ ] usb驱动

- [x] SATA硬盘驱动(AHCI)

- [ ] 驱动程序框架

- [ ] 网卡驱动

- [ ] 网络协议栈

- [ ] 图形驱动

- [x] 第一个进程

- [x] 进程管理

- [ ] IPC进程间通信

- [x] 第一个系统调用函数

- [x] 在物理平台上启动DragonOS（AMD处理器上存在自动重启的问题）

- [x] 多核启动

- [ ] 多核调度及负载均衡

- [x] FAT32文件系统

- [x] VFS虚拟文件系统

- [x] 解析ELF文件格式

- [x] 浮点数支持

- [x] 基于POSIX实现系统调用库

- [x] Shell

- [x] 内核栈反向跟踪

- [ ] 动态加载模块

## 贡献代码

如果你愿意跟我一起开发这个项目，请先发邮件到我的邮箱~

## 贡献者名单

fslongjin

## 联系我

我的邮箱：longjin@RinGoTek.cn

我的博客：[longjin666.cn](https://longjin666.cn)

## 赞赏

如果你愿意的话，点击下面的链接，请我喝杯咖啡吧~请在付款备注处留下您的github ID，我会将其贴到这个页面. 捐赠所得资金将用于网站、论坛社区维护以及一切与本项目所相关的用途。

[捐赠 | 龙进的博客](https://longjin666.cn/?page_id=54)

## 赞赏者列表

- 悟
- [TerryLeeSCUT · GitHub](https://github.com/TerryLeeSCUT)

## 开放源代码声明

本项目采用GPLv2协议进行开源，欢迎您在遵守开源协议的基础之上，使用本项目的代码！

**我们支持**：遵守协议的情况下，利用此项目，创造更大的价值，并为本项目贡献代码。

**我们谴责**：任何不遵守开源协议的行为。包括但不限于：剽窃该项目的代码作为你的毕业设计等学术不端行为以及商业闭源使用而不付费。

若您发现了任何违背开源协议的使用行为，我们欢迎您发邮件反馈！让我们共同建设诚信的开源社区。

## 参考资料

本项目参考了以下资料，我对这些项目、书籍、文档的作者表示感谢！

- 《一个64位操作系统的实现》田宇（人民邮电出版社）

- 《现代操作系统 原理与实现》陈海波、夏虞斌（机械工业出版社）

- [SimpleKernel](https://github.com/Simple-XX/SimpleKernel)

- [osdev.org](https://wiki.osdev.org/Main_Page)

- Multiboot2 Specification version 2.0

- ACPI_6_3_final_Jan30

- the GNU GRUB manual

- Intel® 64 and IA-32 Architectures Software Developer’s Manual

- IA-PC HPET (High Precision Event Timers) Specification

- [skiftOS]([GitHub - skiftOS/skift: 🥑 A hobby operating system built from scratch in modern C++. Featuring a reactive UI library and a strong emphasis on user experience.](https://github.com/skiftOS/skift))

- [GuideOS](https://github.com/Codetector1374/GuideOS)
