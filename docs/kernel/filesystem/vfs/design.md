:::{note}
本文作者: 龙进

Email: <longjin@DragonOS.org>
:::

# 设计


&emsp;&emsp;VFS的架构设计如下图所示：

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
&emsp;&emsp;File结构体是VFS中最基本的抽象，它代表了一个打开的文件。每当进程打开了一个文件，就会创建一个File结构体，用于维护该文件的状态信息。

## 2. Traits

&emsp;&emsp;对于每个具体文件系统，都需要实现以下的trait：

- FileSystem：表明某个struct是一个文件系统
- IndexNode： 表明某个struct是一个索引节点

&emsp;&emsp;一般情况下，FileSystem和IndexNode是一对一的关系，也就是，一个文件系统对应一种IndexNode。但是，对于某些特殊的文件系统，比如DevFS，根据不同的设备类型，会有不同的IndexNode，因此，FileSystem和IndexNode是一对多的关系。

## 3. MountFS

&emsp;&emsp;挂载文件系统虽然实现了FileSystem和IndexNode这两个trait，但它并不是一个“文件系统”，而是一种机制，用于将不同的文件系统挂载到同一个文件系统树上.
所有的文件系统要挂载到文件系统树上，都需要通过MountFS来完成。也就是说，挂载树上的每个文件系统结构体的外面，都套了一层MountFS结构体。

&emsp;&emsp;对于大部分的操作，MountFS都是直接转发给具体的文件系统，而不做任何处理。同时，为了支持跨文件系统的操作，比如在目录树上查找，每次lookup操作或者是find操作，都会通过MountFSInode的对应方法，判断当前inode是否为挂载点，并对挂载点进行特殊处理。如果发现操作跨越了具体文件系统的边界，MountFS就会将操作转发给下一个文件系统，并执行Inode替换。这个功能的实现，也是通过在普通的Inode结构体外面，套一层MountFSInode结构体来实现的。
