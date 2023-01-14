# 引导加载程序

## 原理

&emsp;&emsp;目前，DragonOS支持Legacy BIOS以及UEFI两种方式，进行启动引导。

&emsp;&emsp;在`head.S`的头部包含了Multiboot2引导头，里面标志了一些Multiboot2相关的特定信息，以及一些配置命令。

&emsp;&emsp;在DragonOS的启动初期，会存储由GRUB2传来的magic number以及multiboot2_boot_info_addr。当系统进入`Start_Kernel`函数之后，将会把这两个信息保存到multiboot2驱动程序之中。信息的具体含义请参照Multiboot2 Specification进行理解，该部分难度不大，相信读者经过思考能理解其中的原理。

## 参考资料

- [Multiboot2 Specification](http://git.savannah.gnu.org/cgit/grub.git/tree/doc/multiboot.texi?h=multiboot2)

- [GNU GRUB Manual 2.06](https://www.gnu.org/software/grub/manual/grub/grub.html)

- [UEFI/Legacy启动 - yujianwu - DragonOS社区](https://bbs.dragonos.org/forum.php?mod=viewthread&tid=46)
