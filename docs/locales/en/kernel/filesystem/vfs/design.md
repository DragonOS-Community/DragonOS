:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/filesystem/vfs/design.md

- Translation time: 2025-05-19 01:41:33

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

:::{note}
Author of this article: Long Jin

Email: <longjin@DragonOS.org>
:::

# Design

&emsp;&emsp;The architecture design of VFS is shown in the following diagram:

```text
                      ┌─────────┐
                      │         │
                      │  read   │
            File      │         │
                      │  write  │
             │        │         │
             │        │  ioctl  │
             │        │         │
             │        │  lseek  │
             │        │         │
             │        │  etc..  │
             │        └─────────┘
             │
             ▼        ┌──────────────────────────────────────────────────────────────────────────────┐
            MountFS   │ Maintain the mount tree and handle the mounting of file systems.             │
               │      │    In particular, it handles the "crossing file system boundaries" condition │
               │      │    while doing "lookup" or "find" operations.                                │
               │      └──────────────────────────────────────────────────────────────────────────────┘
               │
               │
               │
Filesystems:   │
               │
               ▼      ┌────────────────────────────────────────────────────────────────────┐
          xxxFSInode  │ Implement corresponding operations based on different file systems │
                      └────────────────────────────────────────────────────────────────────┘
```

## 1. File
&emsp;&emsp;The File structure is the most basic abstraction in VFS, representing an opened file. Whenever a process opens a file, a File structure is created to maintain the state information of that file.

## 2. Traits

&emsp;&emsp;For each specific file system, the following traits must be implemented:

- FileSystem: Indicates that a struct is a file system
- IndexNode: Indicates that a struct is an index node

&emsp;&emsp;Generally, there is a one-to-one relationship between FileSystem and IndexNode, meaning that one file system corresponds to one type of IndexNode. However, for some special file systems, such as DevFS, different IndexNodes may exist based on different device types. Therefore, there is a one-to-many relationship between FileSystem and IndexNode.

## 3. MountFS

&emsp;&emsp;Although MountFS implements the FileSystem and IndexNode traits, it is not itself a "file system," but rather a mechanism used to mount different file systems onto the same file system tree.
All file systems that need to be mounted onto the file system tree must go through MountFS to complete the mounting process. In other words, each file system structure in the mount tree is wrapped with a MountFS structure.

&emsp;&emsp;For most operations, MountFS simply forwards the operation to the specific file system without any processing. At the same time, to support cross-file system operations, such as searching in a directory tree, each lookup or find operation will go through the corresponding method of MountFSInode to determine whether the current inode is a mount point and handle it specially. If the operation is found to cross the boundary of a specific file system, MountFS will forward the operation to the next file system and perform an inode replacement. This functionality is implemented by wrapping a regular Inode structure with a MountFSInode structure.
