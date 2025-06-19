:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/filesystem/overview.md

- Translation time: 2025-05-19 01:41:36

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

:::{note}
Author of this article: Long Jin

Email: <longjin@DragonOS.org>
:::

# Overview

&emsp;&emsp;In this article, we will introduce the architecture design of the DragonOS file system.

## Overview

&emsp;&emsp;As shown in the following diagram, the file system-related mechanisms of DragonOS mainly include the following parts:

- System call interface
- Virtual File System (VFS)
    - File abstraction (File)
    - Mount file system (MountFS)
- Specific file systems

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

## System Call Interface

&emsp;&emsp;The file system-related system call interfaces of DragonOS mainly include the following:

- `sys_open`: Open file
- `sys_read`: Read file
- `sys_write`: Write file
- `sys_close`: Close file
- `sys_lseek`: Set file pointer position
- `sys_mkdir`: Create directory
- `sys_unlink_at`: Delete file or directory (distinguish between file and directory by parameter `flag`)
- `sys_ioctl`: Control device (not implemented)
- `sys_fstat`: Get file status (not implemented)
- `sys_fsync`: Synchronize file (not implemented)
- `sys_ftruncate`: Truncate file (not implemented)
- `sys_fchmod`: Modify file permissions (not implemented)
- Other system call interfaces (not implemented)

&emsp;&emsp;For the specific meaning of the interfaces, you can refer to the relevant documentation of Linux.

## Virtual File System (VFS)

&emsp;&emsp;VFS is the core of the DragonOS file system, providing a unified set of file system interfaces, allowing DragonOS to support various different file systems. The main functions of VFS include:

- Provide a unified file system interface
- Provide file system mounting and unmounting mechanism (MountFS)
- Provide file abstraction (File)
- Provide file system abstraction (FileSystem)
- Provide IndexNode abstraction
- Provide file system caching and synchronization mechanism (not implemented yet)

&emsp;&emsp;For detailed introduction of VFS, please see [DragonOS Virtual File System](vfs/index.rst).

## Specific File Systems

&emsp;&emsp;The file systems currently supported by DragonOS include:

- FAT file system (FAT12, FAT16, FAT32)
- DevFS
- ProcFS
- RamFS
