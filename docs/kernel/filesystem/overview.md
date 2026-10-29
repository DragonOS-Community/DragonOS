::: info Author
Long Jin

Email: `<longjin@DragonOS.org>`
:::


# Overview

In this document, we will introduce the architectural design of the DragonOS file system.

## Summary

As shown in the following diagram, the file system-related mechanisms in DragonOS mainly consist of the following components:

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

For the specific meanings of the interfaces, please refer to the relevant Linux documentation.

## Virtual File System (VFS)

VFS is the core of the DragonOS file system. It provides a unified file system interface, enabling DragonOS to support multiple different file systems. The main functions of VFS include:

- Providing a unified file system interface
- Providing file system mounting and unmounting mechanisms (MountFS)
- Providing file abstraction (File)
- Providing file system abstraction (FileSystem)
- Providing IndexNode abstraction
- Providing file system caching and synchronization mechanisms

For a detailed introduction to VFS, please see [DragonOS Virtual File System](/kernel/filesystem/vfs/).

## Specific File Systems

The file systems currently supported by DragonOS include:

- FAT file systems (FAT12, FAT16, FAT32)
- ext4
- DevFS
- ProcFS
- RamFS
- sysfs
- tmpfs
