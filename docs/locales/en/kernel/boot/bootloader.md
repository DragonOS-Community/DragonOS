:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/boot/bootloader.md

- Translation time: 2025-05-19 01:41:31

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Bootloader

## X86_64

- [x] multiboot2
- [x] HVM/PVH

### HVM/PVH Boot on x86_64

In the DragonOS note segment, there is a PVH header that allows QEMU to boot the DragonOS kernel using the ``-kernel`` parameter.

## RISC-V 64

DragonOS's boot process on RISC-V 64 is as follows:

opensbi --> uboot --> DragonStub --> kernel

This boot process decouples the DragonOS kernel from specific hardware boards, enabling the same binary file to boot and run on different hardware boards.

## Kernel Boot Callbacks

DragonOS abstracts the kernel bootloader, which is represented by the trait ``BootCallbacks``.
Different bootloaders implement the corresponding callback to initialize the kernel's bootParams or other data structures.

When the kernel boots, it automatically registers callbacks based on the type of bootloader. And at the appropriate time, it calls these callback functions.

## References

- [Multiboot2 Specification](http://git.savannah.gnu.org/cgit/grub.git/tree/doc/multiboot.texi?h=multiboot2)

- [GNU GRUB Manual 2.06](https://www.gnu.org/software/grub/manual/grub/grub.html)

- [UEFI/Legacy Boot - yujianwu - DragonOS Community](https://bbs.dragonos.org/forum.php?mod=viewthread&tid=46)
