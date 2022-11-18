# DragonOS虚拟文件系统概述

## 简介

&emsp;&emsp;DragonOS的虚拟文件系统是内核中的一层适配器，为用户程序（或者是系统程序）提供了通用的文件系统接口。同时对内核中的不同文件系统提供了统一的抽象。各种具体的文件系统可以挂载到VFS的框架之中。

&emsp;&emsp;与VFS相关的系统调用有open(), read(), write(), create()等。

### dentry对象

&emsp;&emsp;dentry的全称为directory entry，是VFS中对于目录项的一种抽象数据结构。当读取具体文件系统时，将会由创建dentry对象。dentry对象中包含了指向inode的指针。

&emsp;&emsp;dentry对象为真实文件系统上的目录结构建立了缓存，一旦内存中存在对应路径的dentry对象，我们就能直接获取其中的信息，而不需要进行费时的磁盘操作。请注意，dentry只是为提高文件系统性能而创建的一个缓存，它并不会被写入到磁盘之中。

### inode对象

&emsp;&emsp;inode的全称叫做index node，即索引节点。一般来说，每个dentry都应当包含指向其inode的指针。inode是VFS提供的对文件对象的抽象。inode中的信息是从具体文件系统中读取而来，也可以被刷回具体的文件系统之中。并且，一个inode也可以被多个dentry所引用。

&emsp;&emsp;要查找某个路径下的inode，我们需要调用父目录的inode的lookup()方法。请注意，该方法与具体文件系统有关，需要在具体文件系统之中实现。

### 文件描述符对象

&emsp;&emsp;当一个进程试图通过VFS打开某个文件时，我们需要为这个进程创建文件描述符对象。每个文件对象都会绑定文件的dentry和文件操作方法结构体，还有文件对象的私有信息。

&emsp;&emsp;文件描述符对象中还包含了诸如权限控制、当前访问位置信息等内容，以便VFS对文件进行操作。

&emsp;&emsp;我们对文件进行操作都会使用到文件描述符，具体来说，就是要调用文件描述符之中的file_ops所包含的各种方法。

---

## 注册文件系统到VFS

&emsp;&emsp;如果需要注册或取消注册某个具体文件系统到VFS之中，则需要以下两个接口：

```c
#include<filesystem/VFS/VFS.h>

uint64_t vfs_register_filesystem(struct vfs_filesystem_type_t *fs);
uint64_t vfs_unregister_filesystem(struct vfs_filesystem_type_t *fs);
```

&emsp;&emsp;这里需要通过`struct vfs_filesystem_type_t`来描述具体的文件系统。

### struct  vfs_filesystem_type_t

&emsp;&emsp;这个数据结构描述了具体文件系统的一些信息。当我们挂载具体文件系统的时候，将会调用它的read_superblock方法，以确定要被挂载的文件系统的具体信息。

&emsp;&emsp;该数据结构的定义在`kernel/filesystem/VFS/VFS.h`中，结构如下：

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

&emsp;&emsp;文件系统名称字符串

**fs_flags**

&emsp;&emsp;文件系统的一些标志位。目前，DragonOS尚未实现相关功能。

**read_superblock**

&emsp;&emsp;当新的文件系统实例将要被挂载时，将会调用此方法，以读取具体的实例的信息。

**next**

&emsp;&emsp;指向链表中下一个`struct vfs_filesystem_type_t`的指针。

---

## 超级块(superblock)对象

&emsp;&emsp;一个超级块对象代表了一个被挂载到VFS中的具体文件系统。

### struct vfs_superblock_t

&emsp;&emsp;该数据结构为超级块结构体。

&emsp;&emsp;该数据结构定义在`kernel/filesystem/VFS/VFS.h`中，结构如下：

```c
struct vfs_superblock_t
{
    struct vfs_dir_entry_t *root;
    struct vfs_super_block_operations_t *sb_ops;
    void *private_sb_info;
};
```

**root**

&emsp;&emsp;该具体文件系统的根目录的dentry

**sb_ops**

&emsp;&emsp;该超级块对象的操作方法。

**private_sb_info**

&emsp;&emsp;超级块的私有信息。包含了具体文件系统的私有的、全局性的信息。

### struct vfs_super_block_operations_t

&emsp;&emsp;该数据结构为超级块的操作接口。VFS通过这些接口来操作具体的文件系统的超级块。

&emsp;&emsp;该数据结构定义在`kernel/filesystem/VFS/VFS.h`中，结构如下：

```c
struct vfs_super_block_operations_t
{
    void (*write_superblock)(struct vfs_superblock_t *sb);
    void (*put_superblock)(struct vfs_superblock_t *sb);
    void (*write_inode)(struct vfs_index_node_t *inode); // 将inode信息写入磁盘
};
```

**write_superblock**

&emsp;&emsp;将superblock中的信息写入磁盘

**put_superblock**

&emsp;&emsp;释放超级块

**write_inode**

&emsp;&emsp;将inode的信息写入磁盘

---

## 索引结点(inode)对象

&emsp;&emsp;每个inode对象代表了具体的文件系统之中的一个对象（目录项）。

### struct vfs_index_node_t

&emsp;&emsp;该数据结构为inode对象的数据结构，与文件系统中的具体的文件结点对象具有一对一映射的关系。

&emsp;&emsp;该数据结构定义在`kernel/filesystem/VFS/VFS.h`中，结构如下：

```c
struct vfs_index_node_t
{
    uint64_t file_size; // 文件大小
    uint64_t blocks;    // 占用的扇区数
    uint64_t attribute;

    struct vfs_superblock_t *sb;
    struct vfs_file_operations_t *file_ops;
    struct vfs_inode_operations_t *inode_ops;

    void *private_inode_info;
};
```

**file_size**

&emsp;&emsp;文件的大小。若为文件夹，则该值为文件夹内所有文件的大小总和（估计值）。

**blocks**

&emsp;&emsp;文件占用的磁盘块数（扇区数）

**attribute**

&emsp;&emsp;inode的属性。可选值如下：

> - VFS_IF_FILE
> 
> - VFS_IF_DIR
> 
> - VFS_IF_DEVICE

**sb**

&emsp;&emsp;指向文件系统超级块的指针

**file_ops**

&emsp;&emsp;当前文件的操作接口

**inode_ops**

&emsp;&emsp;当前inode的操作接口

**private_inode_info**

&emsp;&emsp;与具体文件系统相关的inode信息。该部分由具体文件系统实现，包含该inode在具体文件系统之中的特定格式信息。

### struct vfs_inode_operations_t

&emsp;&emsp;该接口为inode的操作方法接口，由具体文件系统实现。并与具体文件系统之中的inode相互绑定。

&emsp;&emsp;该接口定义于`kernel/filesystem/VFS/VFS.h`中，结构如下：

```c
struct vfs_inode_operations_t
{
    long (*create)(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dest_dEntry, int mode);
    struct vfs_dir_entry_t *(*lookup)(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dest_dEntry);
    long (*mkdir)(struct vfs_index_node_t *inode, struct vfs_dir_entry_t *dEntry, int mode);
    long (*rmdir)(struct vfs_index_node_t *inode, struct vfs_dir_entry_t *dEntry);
    long (*rename)(struct vfs_index_node_t *old_inode, struct vfs_dir_entry_t *old_dEntry, struct vfs_index_node_t *new_inode, struct vfs_dir_entry_t *new_dEntry);
    long (*getAttr)(struct vfs_dir_entry_t *dEntry, uint64_t *attr);
    long (*setAttr)(struct vfs_dir_entry_t *dEntry, uint64_t *attr);
};
```

**create**

&emsp;&emsp;在父节点下，创建一个新的inode，并绑定到dest_dEntry上。

&emsp;&emsp;该函数的应当被`sys_open()`系统调用在使用了`O_CREAT`选项打开文件时调用，从而创建一个新的文件。请注意，传递给create()函数的`dest_dEntry`参数不应包含一个inode，也就是说，inode对象应当被具体文件系统所创建。

**lookup**

&emsp;&emsp;当VFS需要在父目录中查找一个inode的时候，将会调用lookup方法。被查找的目录项的名称将会通过dest_dEntry传给lookup方法。

&emsp;&emsp;若lookup方法找到对应的目录项，将填充完善dest_dEntry对象。否则，返回NULL。

**mkdir**

&emsp;&emsp;该函数被mkdir()系统调用所调用，用于在inode下创建子目录，并将子目录的inode绑定到dEntry对象之中。

**rmdir**

&emsp;&emsp;该函数被rmdir()系统调用所调用，用于删除给定inode下的子目录项。

**rename**

&emsp;&emsp;该函数被rename系统调用（尚未实现）所调用，用于将给定的目录项重命名。

**getAttr**

&emsp;&emsp;用来获取目录项的属性。

**setAttr**

&emsp;&emsp;用来设置目录项的属性