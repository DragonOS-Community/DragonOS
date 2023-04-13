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

&emsp;&emsp;DragonOS的文件系统相关的系统调用接口主要包括以下几个：

- `sys_open`：打开文件
- `sys_read`：读取文件
- `sys_write`：写入文件
- `sys_close`：关闭文件
- `sys_lseek`：定位文件指针
- `sys_mkdir`：创建目录
- `sys_unlink_at`：删除文件或目录（通过参数`flag`区分到底是删除文件还是目录）
- `sys_ioctl`：控制设备 （未实现）
- `sys_fstat`：获取文件状态（未实现）
- `sys_fsync`：同步文件（未实现）
- `sys_ftruncate`：截断文件（未实现）
- `sys_fchmod`：修改文件权限（未实现）
- 其他系统调用接口（未实现）

&emsp;&emsp;关于接口的具体含义，可以参考 [DragonOS系统调用接口](../../syscall_api/index.rst)。

## 虚拟文件系统（VFS）

&emsp;&emsp;VFS是DragonOS文件系统的核心，它提供了一套统一的文件系统接口，使得DragonOS可以支持多种不同的文件系统。VFS的主要功能包括：

- 提供统一的文件系统接口
- 提供文件系统的挂载和卸载机制（MountFS）
- 提供文件抽象（File）
- 提供文件系统的抽象（FileSystem）
- 提供IndexNode抽象
- 提供文件系统的缓存、同步机制（尚未实现）


&emsp;&emsp;关于VFS的详细介绍，请见[DragonOS虚拟文件系统](vfs/index.rst)。

## 具体的文件系统

&emsp;&emsp;DragonOS目前支持的文件系统包括：

- FAT文件系统（FAT12、FAT16、FAT32）
- DevFS
- ProcFS
- RamFS
