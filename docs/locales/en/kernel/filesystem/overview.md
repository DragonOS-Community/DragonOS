:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/filesystem/overview.md

- Translation time: 2026-01-15 12:54:32

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

:::{note}
Author: Long Jin

Email: <longjin@DragonOS.org>
:::

# Overview

&emsp;&emsp;In this document, we will introduce the architectural design of the DragonOS file system.

## Summary

&emsp;&emsp;As shown in the following diagram, the file system-related mechanisms in DragonOS mainly consist of the following components:

- System call interface
- Virtual file system
    - File abstraction (File)
    - Mounted file system (MountFS)
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

&emsp;&emsp;For the specific meanings of the interfaces, please refer to the relevant Linux documentation.

## Virtual File System (VFS)

&emsp;&emsp;VFS is the core of the DragonOS file system. It provides a unified file system interface, enabling DragonOS to support multiple different file systems. The main functions of VFS include:

- Providing a unified file system interface
- Providing file system mounting and unmounting mechanisms (MountFS)
- Providing file abstraction (File)
- Providing file system abstraction (FileSystem)
- Providing IndexNode abstraction
- Providing file system caching and synchronization mechanisms

&emsp;&emsp;For a detailed introduction to VFS, please see [DragonOS Virtual File System](vfs/index.rst).

## Specific File Systems

&emsp;&emsp;The file systems currently supported by DragonOS include:

- FAT file systems (FAT12, FAT16, FAT32)
- ext4
- DevFS
- ProcFS
- RamFS
- sysfs
- tmpfs
