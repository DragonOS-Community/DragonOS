# DragonOS虚拟文件系统概述

## 简介

​    DragonOS的虚拟文件系统是内核中的一层适配器，为用户程序（或者是系统程序）提供了通用的文件系统接口。同时对内核中的不同文件系统提供了统一的抽象。各种具体的文件系统可以挂载到VFS的框架之中。

​    与VFS相关的系统调用有open(), read(), write(), create()等。

### dentry对象

​    dentry的全称为directory entry，是VFS中对于目录项的一种抽象数据结构。当读取具体文件系统时，将会由创建dentry对象。dentry对象中包含了指向inode的指针。

​    dentry对象为真实文件系统上的目录结构建立了缓存，一旦内存中存在对应路径的dentry对象，我们就能直接获取其中的信息，而不需要进行费时的磁盘操作。请注意，dentry只是为提高文件系统性能而创建的一个缓存，它并不会被写入到磁盘之中。

### inode对象

​    inode的全称叫做index node，即索引节点。一般来说，每个dentry都应当包含指向其inode的阵阵。inode是VFS提供的对文件对象的抽象。inode中的信息是从具体文件系统中读取而来，也可以被刷回具体的文件系统之中。并且，一个inode也可以被多个dentry所引用。

​    要查找某个路径下的inode，我们需要调用父目录的inode的lookup()方法。请注意，该方法与具体文件系统有关，需要在具体文件系统之中实现。

### 文件描述符对象

​    当一个进程试图通过VFS打开某个文件时，我们需要为这个进程创建文件描述符对象。每个文件对象都会绑定文件的dentry和文件操作方法结构体，还有文件对象的私有信息。

​    文件描述符对象中还包含了诸如权限控制、当前访问位置信息等内容，以便VFS对文件进行操作。

​    我们对文件进行操作都会使用到文件描述符，具体来说，就是要调用文件描述符之中的file_ops所包含的各种方法。

## 挂载文件系统到VFS

​    如果需要注册或取消注册某个具体文件系统到VFS之中，则需要以下两个接口：

```c
#include<filesystem/VFS/VFS.h>

uint64_t vfs_register_filesystem(struct vfs_filesystem_type_t *fs);
uint64_t vfs_unregister_filesystem(struct vfs_filesystem_type_t *fs);
```

​    这里需要通过`struct vfs_filesystem_type_t`来描述具体的文件系统。

### struct  vfs_filesystem_type_t

​    这个数据结构描述了具体文件系统的一些信息。当我们挂载具体文件系统的时候，将会调用它的read_superblock方法，以确定要被挂载的文件系统的具体信息。

​    该数据结构的定义在`kernel/filesystem/VFS/VFS.h`中，结构如下：

```c
struct vfs_filesystem_type_t
{
    char *name;
    int fs_flags;
    // 解析文件系统引导扇区的函数，为文件系统创建超级块结构。其中DPTE为磁盘分区表entry（MBR、GPT不同）
    struct vfs_superblock_t *(*read_superblock)(void *DPTE, uint8_t DPT_type, void *buf, int8_t ahci_ctrl_num, int8_t ahci_port_num, int8_t part_num); 
    struct vfs_filesystem_type_t *next;
};
```

**name**

​    文件系统名称字符串

**fs_flags**

​    文件系统的一些标志位。目前，DragonOS尚未实现相关功能。

**read_superblock**

​    当新的文件系统实例将要被挂载时，将会调用此方法，以读取具体的实例的信息。

**next**

​    指向链表中下一个`struct vfs_filesystem_type_t`的指针。