# 引导加载程序

## X86_64

- [x] multiboot2

## RISC-V 64

DragonOS在RISC-V 64上，启动流程为：

opensbi --> uboot --> DragonStub --> kernel

这个启动流程，使得DragonOS内核与具体的硬件板卡解耦，能够以同一个二进制文件，在不同的硬件板卡上启动运行。


## 内核启动回调

DragonOS对内核引导加载程序进行了抽象，体现为`BootCallbacks`这个trait。
不同的引导加载程序，实现对应的callback，初始化内核bootParams或者是其他的一些数据结构。

内核启动时，自动根据引导加载程序的类型，注册回调。并且在适当的时候，会调用这些回调函数。

## 参考资料

- [Multiboot2 Specification](http://git.savannah.gnu.org/cgit/grub.git/tree/doc/multiboot.texi?h=multiboot2)

- [GNU GRUB Manual 2.06](https://www.gnu.org/software/grub/manual/grub/grub.html)

- [UEFI/Legacy启动 - yujianwu - DragonOS社区](https://bbs.dragonos.org/forum.php?mod=viewthread&tid=46)
