(_core_mm_api)=

# 内存管理API

## SLAB内存池

SLAB内存池提供小内存对象的分配功能。

### `void *kmalloc(unsigned long size, gfp_t gfp)`

&emsp;&emsp;获取小块的内存。

#### 描述

&emsp;&emsp;kmalloc用于获取那些小于2M内存页大小的内存对象。可分配的内存对象大小为32bytes~1MBytes. 且分配的内存块大小、起始地址按照2的n次幂进行对齐。（比如，申请的是80bytes的内存空间，那么获得的内存对象大小为128bytes且内存地址按照128bytes对齐）

##### 参数

**size**

&emsp;&emsp;内存对象的大小

**gfp**

&emsp;&emsp;标志位

### `void *kzalloc(unsigned long size, gfp_t gfp)`

#### 描述

&emsp;&emsp;获取小块的内存，并将其清零。其余功能与kmalloc相同。


##### 参数

**size**

&emsp;&emsp;内存对象的大小

**gfp**

&emsp;&emsp;标志位

### `unsigned long kfree(void *address)`

&emsp;&emsp;释放从slab分配的内存。

#### 描述

&emsp;&emsp;该函数用于释放通过kmalloc申请的内存。如果`address`为NULL，则函数被调用后，无事发生。

&emsp;&emsp;请不要通过这个函数释放那些不是从`kmalloc()`或`kzalloc()`申请的内存，否则将会导致系统崩溃。

##### 参数

**address**

&emsp;&emsp;指向内存对象的起始地址的指针

## 物理页管理

DragonOS支持对物理页的直接操作

### `struct Page *alloc_pages(unsigned int zone_select, int num, ul flags)`

#### 描述

&emsp;&emsp;从物理页管理单元中申请一段连续的物理页

#### 参数

**zone_select**

&emsp;&emsp;要申请的物理页所位于的内存区域

可选值：

- `ZONE_DMA` DMA映射专用区域
- `ZONE_NORMAL` 正常的物理内存区域，已在页表高地址处映射
- `ZONE_UNMAPPED_IN_PGT` 尚未在页表中映射的区域

**num**

&emsp;&emsp;要申请的连续物理页的数目，该值应当小于64

**flags**

&emsp;&emsp;分配的页面要被设置成的属性

可选值：

- `PAGE_PGT_MAPPED` 页面在页表中已被映射
- `PAGE_KERNEL_INIT` 内核初始化所占用的页
- `PAGE_DEVICE` 设备MMIO映射的内存
- `PAGE_KERNEL` 内核层页
- `PAGE_SHARED` 共享页

#### 返回值

##### 成功

&emsp;&emsp;成功申请则返回指向起始页面的Page结构体的指针

##### 失败

&emsp;&emsp;当ZONE错误或内存不足时，返回`NULL`

### `void free_pages(struct Page *page, int number)`

#### 描述

&emsp;&emsp;从物理页管理单元中释放一段连续的物理页。

#### 参数

**page**

&emsp;&emsp;要释放的第一个物理页的Page结构体

**number**

&emsp;&emsp;要释放的连续内存页的数量。该值应小于64

## 页表管理

### `int mm_map_phys_addr(ul virt_addr_start, ul phys_addr_start, ul length, ul flags, bool use4k)`

#### 描述

&emsp;&emsp;将一段物理地址映射到当前页表的指定虚拟地址处

#### 参数

**virt_addr_start**

&emsp;&emsp;虚拟地址的起始地址

**phys_addr_start**

&emsp;&emsp;物理地址的起始地址

**length**

&emsp;&emsp;要映射的地址空间的长度

**flags**

&emsp;&emsp;页表项的属性

**use4k**

&emsp;&emsp;使用4级页表，将地址区域映射为若干4K页

### `int mm_map_proc_page_table(ul proc_page_table_addr, bool is_phys, ul virt_addr_start, ul phys_addr_start, ul length, ul flags, bool user, bool flush, bool use4k)`

#### 描述

&emsp;&emsp;将一段物理地址映射到指定页表的指定虚拟地址处

#### 参数

**proc_page_table_addr**

&emsp;&emsp;指定的顶层页表的起始地址

**is_phys**

&emsp;&emsp;该顶层页表地址是否为物理地址

**virt_addr_start**

&emsp;&emsp;虚拟地址的起始地址

**phys_addr_start**

&emsp;&emsp;物理地址的起始地址

**length**

&emsp;&emsp;要映射的地址空间的长度

**flags**

&emsp;&emsp;页表项的属性

**user**

&emsp;&emsp;页面是否为用户态可访问

**flush**

&emsp;&emsp;完成映射后，是否刷新TLB

**use4k**

&emsp;&emsp;使用4级页表，将地址区域映射为若干4K页

#### 返回值

- 映射成功：0
- 映射失败：-EFAULT

### `void mm_unmap_proc_table(ul proc_page_table_addr, bool is_phys, ul virt_addr_start, ul length)`

#### 描述

&emsp;&emsp;取消给定页表中的指定地址空间的页表项映射。

#### 参数

**proc_page_table_addr**

&emsp;&emsp;指定的顶层页表的基地址

**is_phys**

&emsp;&emsp;该顶层页表地址是否为物理地址

**virt_addr_start**

&emsp;&emsp;虚拟地址的起始地址

**length**

&emsp;&emsp;要取消映射的地址空间的长度

### `mm_unmap_addr(virt_addr, length)`

#### 描述

&emsp;&emsp;该宏定义用于取消当前进程的页表中的指定地址空间的页表项映射。

#### 参数

**virt_addr**

&emsp;&emsp;虚拟地址的起始地址

**length**

&emsp;&emsp;要取消映射的地址空间的长度

## 内存信息获取

### `struct mm_stat_t mm_stat()`

#### 描述

&emsp;&emsp;获取计算机目前的内存空间使用情况

#### 参数

无

#### 返回值

&emsp;&emsp;返回值是一个`mm_mstat_t`结构体，该结构体定义于`mm/mm.h`中。其中包含了以下信息（单位均为字节）：

| 参数名        | 解释                      |
| ---------- | ----------------------- |
| total      | 计算机的总内存数量大小             |
| used       | 已使用的内存大小                |
| free       | 空闲物理页所占的内存大小            |
| shared     | 共享的内存大小                 |
| cache_used | 位于slab缓冲区中的已使用的内存大小     |
| cache_free | 位于slab缓冲区中的空闲的内存大小      |
| available  | 系统总空闲内存大小（包括kmalloc缓冲区） |