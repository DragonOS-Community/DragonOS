:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: introduction/features.md

- Translation time: 2025-05-19 01:42:30

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

(_translated_label___genreal_features_en)=

# Features of DragonOS

## Specifications

- [x] Bootloader: Multiboot2

- [x] Interface: POSIX 2008

## Kernel Layer

### Memory Management

- [x] Page Frame Allocator
- [x] Small Object Allocator
- [x] VMA (Virtual Memory Area)
- [x] Automatic MMIO Address Space Allocation
- [x] Page Mapper
- [x] Hardware Abstraction Layer
- [x] Independent User Address Space Management Mechanism
- [x] C Interface Compatibility Layer

### Multicore

- [x] Multicore Boot
- [x] IPI (Inter-Processor Interrupt) Framework

### Process Management

- [x] Process Creation
- [x] Process Reclamation
- [x] Kernel Threads
- [x] Fork
- [x] Exec
- [x] Process Sleep (Supports High-Precision Sleep)
- [x] Kthread Mechanism
- [x] Extensible Binary Loader

#### Synchronization Primitives

- [x] Mutex
- [x] Semaphore
- [x] Atomic Variables
- [x] Spinlock
- [x] Wait Queue

### Scheduling

- [x] CFS Scheduler
- [x] Real-Time Scheduler (FIFO, RR)
- [x] Single-Core Scheduling
- [x] Multi-Core Scheduling
- [x] Load Balancing

### IPC (Inter-Process Communication)

- [x] Anonymous Pipe
- [x] Signal

### File System

- [x] VFS (Virtual File System)
- [x] FAT12/16/32
- [x] Devfs
- [x] RamFS
- [x] Procfs
- [x] Sysfs

### Exception and Interrupt Handling

- [x] APIC
- [x] Softirq (Soft Interrupt)
- [x] Kernel Stack Traceback

### Kernel Utility Library

- [x] String Operation Library
- [x] ELF Executable Support
- [x] printk
- [x] Basic Math Library
- [x] Screen Manager
- [x] TextUI Framework
- [x] CRC Function Library
- [x] Notification Chain

### System Calls

&emsp;&emsp;[See System Call Documentation](https://docs.dragonos.org/zh_CN/latest/syscall_api/index.html)

### Test Framework

- [x] ktest

### Drivers

- [x] ACPI (Advanced Configuration and Power Interface) Module
- [x] IDE Hard Disk
- [x] AHCI Hard Disk
- [x] PCI, PCIe Bus
- [x] XHCI (USB 3.0)
- [x] PS/2 Keyboard
- [x] PS/2 Mouse
- [x] HPET (High Precision Event Timer)
- [x] RTC (Real-Time Clock)
- [x] Local APIC Timer
- [x] UART Serial Port
- [x] VBE (Video BIOS Extension) Display
- [x] VirtIO Network Card
- [x] x87 FPU
- [x] TTY Terminal
- [x] Floating Point Processor

## User Layer

### LibC

- [x] Basic System Calls
- [x] Basic Standard Library Functions
- [x] Partial Mathematical Functions

### Shell Command Line Programs

- [x] Parsing Based on String Matching
- [x] Basic Commands

### Http Server

- A simple Http Server written in C, capable of running static websites.

## Software Portability

- [x] GCC 11.3.0 (Currently only supports x86_64 Cross Compiler) [https://github.com/DragonOS-Community/gcc](https://github.com/DragonOS-Community/gcc)
- [x] binutils 2.38 (Currently only supports x86_64 Cross Compiler) [https://github.com/DragonOS-Community/binutils](https://github.com/DragonOS-Community/binutils)
- [x] gmp 6.2.1 [https://github.com/DragonOS-Community/gmp-6.2.1](https://github.com/DragonOS-Community/gmp-6.2.1)
- [x] mpfr 4.1.1 [https://github.com/DragonOS-Community/mpfr](https://github.com/DragonOS-Community/mpfr)
- [x] mpc 1.2.1 [https://github.com/DragonOS-Community/mpc](https://github.com/DragonOS-Community/mpc)
- [x] relibc [https://github.com/DragonOS-Community/relibc](https://github.com/DragonOS-Community/relibc)
- [x] sqlite3
