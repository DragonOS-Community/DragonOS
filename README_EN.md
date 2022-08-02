# DragonOS

**Languages** [中文](README.md)|English

&nbsp;

This project is a operating system running on computer which is in X86_ 64 Architecture . The DragonOS is currently under development!

## Websites
- Home Page  **[DragonOS.org](https://dragonos.org)**
- Documentation  **[docs.DragonOS.org](https://docs.dragonos.org)**
- BBS  **[bbs.DragonOS.org](https://bbs.dragonos.org)**
- QQ group **115763565**
- code search engine [code.DragonOS.org](http://code.dragonos.org)&nbsp;

## Development Environment

GCC>=8.0

qemu==6.2

grub==2.06

## How to run?

1. clone the project

2. Run the <u>run.sh</u> 

## To do list:

- [x] multiboot2

- [x] printk

- [x] Simple exception capture and interrupt handling

- [x] APIC

- [x] Primary memory management unit

- [x] SLAB memory pool

- [x] PS/2 Keyboard and mouse driver

- [x] PCI bus driver

- [ ] USB Driver

- [x] SATA Hard disk driver(AHCI)

- [ ] Driver Framework

- [ ] Network card driver

- [ ] Internet protocol stack

- [ ] Graphics driver

- [x] First process

- [x] Process management

- [ ] IPC

- [x] First system call function

- [x] Start dragonos on the physical platform (There is a bug which can make the computer automatically reboot on AMD processor)

- [x] Multi core boot

- [ ] Multi core scheduling and load balancing

- [x] FAT32 file system

- [x] virtual file system

- [x] Parsing ELF file format

- [x] Floating point support

- [x] Implementation of system call library based on POSIX

- [x] Shell

- [x] Kernel stack backtracking

- [ ] Dynamic loading module

## Contribute code

If you are willing to develop this project with me, please email me first~

## List of contributors

fslongjin

## Contact with me

Email：longjin@RinGoTek.cn

Blog：[longjin666.cn](https://longjin666.cn)

## Reward

If you like, click the link below and buy me a cup of coffee ~ please leave your GitHub ID in the payment remarks and I will post it to this page. The donated funds will be used for website, forum community maintenance and all purposes related to the project.

[The reward webpage](https://longjin666.cn/?page_id=54)

## Sponsors

- 悟
- [TerryLeeSCUT · GitHub](https://github.com/TerryLeeSCUT)

## Open source statement

This project adopts GPLv2 LICENSE for open source. You are welcome to use the code of this project on the basis of abiding by the open source license!
**What we support:** using this project to create greater value and contribute code to this project under the condition of abiding by the agreement.
**What we condemn**: any non-compliance with the open source license. Including but not limited to: plagiarizing the code of the project as your graduation project and other academic misconduct, as well as commercial closed source use without payment.
If you find any violation of the open source license, we welcome you to send email feedback! Let's build an honest open source community together!

## References

This project refers to the following materials. I sincerely give my thanks to the authors of these projects, books and documents!

- Implementation of a 64 bit operating system, Tian Yu (POSTS&TELECOM  PRESS)

- Principle and implementation of modern operating system, Chen Haibo, Xia Yubin (China Machine Press)

- [SimpleKernel](https://github.com/Simple-XX/SimpleKernel)

- [osdev.org](https://wiki.osdev.org/Main_Page)

- Multiboot2 Specification version 2.0

- ACPI_6_3_final_Jan30

- the GNU GRUB manual

- Intel® 64 and IA-32 Architectures Software Developer’s Manual

- IA-PC HPET (High Precision Event Timers) Specification

- [skiftOS]([GitHub - skiftOS/skift: 🥑 A hobby operating system built from scratch in modern C++. Featuring a reactive UI library and a strong emphasis on user experience.](https://github.com/skiftOS/skift))

- [GuideOS](https://github.com/Codetector1374/GuideOS)
