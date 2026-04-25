:::{note}
本文作者: 龙进

Email: <longjin@DragonOS.org>
:::

# 概述

&emsp;&emsp;在本文中，我们将介绍DragonOS文件系统的架构设计。

## 总览

&emsp;&emsp;如下图所示，DragonOS的文件系统相关的机制主要包括以下几个部分：

- 系统调用接口
- 虚拟文件系统
    - 文件抽象（File）
    - 挂载文件系统（MountFS）
- 具体的文件系统

```text
            ┌─────────────────────────────────────────────────┐
            │                                                 │
Syscall:    │   sys_open, sys_read, sys_write, sys_close,     │
            │                                                 │
            │   sys_lseek, etc..                              │
            │                                                 │
            └───────────────────────┬─────────────────────────┘
                                    │
                                    │
    VFS:                     ┌──────▼─────┐
                             │            │
                             │    File    │
                             │            │
                             └──────┬─────┘
                                    │
                           ┌────────▼────────┐
                           │                 │
                           │     MountFS     │
                           │                 │
                           └────┬────────────┘
                                │
   Filesystems:   ┌─────────────┼─────────────┬────────────┐
                  │             │             │            │
            ┌─────▼─────┐ ┌─────▼─────┐ ┌─────▼────┐ ┌─────▼─────┐
            │           │ │           │ │          │ │           │
            │    FAT    │ │   DevFS   │ │  ProcFS  │ │   RamFS   │
            │           │ │           │ │          │ │           │
            └───────────┘ └───────────┘ └──────────┘ └───────────┘
```

## 系统调用接口


&emsp;&emsp;关于接口的具体含义，可以参考Linux的相关文档。

## 虚拟文件系统（VFS）

&emsp;&emsp;VFS是DragonOS文件系统的核心，它提供了一套统一的文件系统接口，使得DragonOS可以支持多种不同的文件系统。VFS的主要功能包括：

- 提供统一的文件系统接口
- 提供文件系统的挂载和卸载机制（MountFS）
- 提供文件抽象（File）
- 提供文件系统的抽象（FileSystem）
- 提供IndexNode抽象
- 提供文件系统的缓存、同步机制


&emsp;&emsp;关于VFS的详细介绍，请见[DragonOS虚拟文件系统](vfs/index.rst)。

## 具体的文件系统

&emsp;&emsp;DragonOS目前支持的文件系统包括：

- FAT文件系统（FAT12、FAT16、FAT32）
- ext4
- DevFS
- ProcFS
- RamFS
- sysfs
- tmpfs