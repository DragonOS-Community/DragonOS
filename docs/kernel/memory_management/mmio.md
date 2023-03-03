# MMIO

MMIO是“内存映射IO”的缩写，它被广泛应用于与硬件设备的交互之中。

## 地址空间管理

DragonOS中实现了MMIO地址空间的管理机制，本节将介绍它们。

### 为什么需要MMIO地址空间自动分配？

&emsp;&emsp;由于计算机上的很多设备都需要MMIO的地址空间，而每台计算机上所连接的各种设备的对MMIO地址空间的需求是不一样的。如果我们为每个类型的设备都手动指定一个MMIO地址，会使得虚拟地址空间被大大浪费，也会增加系统的复杂性。并且，我们在将来还需要为不同的虚拟内存区域做异常处理函数。因此，我们需要一套能够自动分配MMIO地址空间的机制。

### 这套机制提供了什么功能？

- 为驱动程序分配4K到1GB的MMIO虚拟地址空间
- 对于这些虚拟地址空间，添加到VMA中进行统一管理
- 可以批量释放这些地址空间

### 这套机制是如何实现的？

&emsp;&emsp;这套机制本质上是使用了伙伴系统来对MMIO虚拟地址空间进行维护。在`mm/mm.h`中指定了MMIO的虚拟地址空间范围，这个范围是`0xffffa10000000000`开始的1TB的空间。也就是说，这个伙伴系统为MMIO维护了这1TB的虚拟地址空间。

### 地址空间分配过程

1. 初始化MMIO-mapping模块，在mmio的伙伴系统中创建512个1GB的`__mmio_buddy_addr_region`
2. 驱动程序使用`mmio_create`请求分配地址空间。
3. `mmio_create`对申请的地址空间大小按照2的n次幂进行对齐，然后从buddy中申请内存地址空间
4. 创建VMA，并将VMA标记为`VM_IO|VM_DONTCOPY`。MMIO的vma只绑定在`initial_mm`下，且不会被拷贝。
5. 分配完成

一旦MMIO地址空间分配完成，它就像普通的vma一样，可以使用mmap系列函数进行操作。

### MMIO的映射过程

&emsp;&emsp;在得到了虚拟地址空间之后，当我们尝试往这块地址空间内映射内存时，我们可以调用`mm_map`函数，对这块区域进行映射。

&emsp;&emsp;该函数会对MMIO的VMA的映射做出特殊处理。即：创建`Page`结构体以及对应的`anon_vma`. 然后会将对应的物理地址，填写到页表之中。

### MMIO虚拟地址空间的释放

&emsp;&emsp;当设备被卸载时，驱动程序可以调用`mmio_release`函数对指定的mmio地址空间进行释放。

&emsp;&emsp;释放的过程中，`mmio_release`将执行以下流程：

1. 取消mmio区域在页表中的映射。
2. 将释放MMIO区域的VMA
3. 将地址空间归还给mmio的伙伴系统。

## MMIO的伙伴算法

### 伙伴算法

&emsp;&emsp;伙伴（buddy）算法的作用是维护以及组织大块连续内存块的分配和回收，以减少系统时运行产生的外部碎片。伙伴系统中的每个内存块的大小均为$2^n$。在DragonOS中，伙伴系统内存池共维护了1TB的连续存储空间，最大的内存块大小为1G，即 $2^{30}$ bit，最小的内存块大小为4K，即 $2^{12}$ bit。

### 伙伴的定义

&emsp;&emsp;同时满足以下三个条件的两个内存块被称为伙伴内存块：

1. 两个内存块的大小相同
2. 两个内存块的内存地址连续
3. 两个内存块由同一个大块内存分裂得到

### 伙伴算法的数据结构

```

                                  MmioBuddyMemPool

┌─────────────────────────────────────────────────────────────────────────────────────┐
│                                                                                     │
│                                 pool_start_addr                                     │
│                                                                                     │
├─────────────────────────────────────────────────────────────────────────────────────┤
│                                                                                     │
│                                    pool_size                                        │
│                                                                                     │
├─────────────────────────────────────────────────────────────────────────────────────┤
│                                                                                     │
│                                                                                     │
│                                   free_regions                                      │
│                                                                                     │
│                                  ┌────────────┐                                     │
│                                  │            │     ┌───────┐     ┌────────┐        │
│                                  │ ┌────────┬─┼────►│       ├────►│        │        │
│                                  │ │  list  │ │     │ vaddr │     │ vaddr  │        │
│                                  │ │        │◄├─────┤       │◄────┤        │        │
│                  MmioFreeRegionList├────────┤ │     └───────┘     └────────┘        │
│                                  │ │num_free│ │                                     │
│                                  │ └────────┘ │  MmioBuddyAddrRegion                │
│          MMIO_BUDDY_MIN_EXP - 12 │      0     │                                     │
│                                  ├────────────┤                                     │
│                                  │      1     │                                     │
│                                  ├────────────┤                                     │
│                                  │      2     │                                     │
│                                  ├────────────┤                                     │
│                                  │      3     │                                     │
│                                  ├────────────┤                                     │
│                                  │     ...    │                                     │
│                                  ├────────────┤                                     │
│                                  │     ...    │                                     │
│                                  ├────────────┤                                     │
│          MMIO_BUDDY_MAX_EXP - 12 │     18     │                                     │
│                                  └────────────┘                                     │
│                                                                                     │
│                                                                                     │
│                                                                                     │
└─────────────────────────────────────────────────────────────────────────────────────┘
```

```rust

// 最大的内存块为1G，其幂为30
const MMIO_BUDDY_MAX_EXP: u32 = PAGE_1G_SHIFT;
// 最小的内存块为4K，其幂为12                                  
const MMIO_BUDDY_MIN_EXP: u32 = PAGE_4K_SHIFT;
// 内存池数组的大小为18
const MMIO_BUDDY_REGION_COUNT: u32 = MMIO_BUDDY_MAX_EXP - MMIO_BUDDY_MIN_EXP + 1;

/// buddy内存池
pub struct MmioBuddyMemPool {
    // 内存池的起始地址
    pool_start_addr: u64, 
    // 内存池大小：初始化为1TB
    pool_size: u64,
    // 空闲内存块链表数组
    // MMIO_BUDDY_REGION_COUNT = MMIO_BUDDY_MAX_EXP - MMIO_BUDDY_MIN_EXP + 1
    free_regions: [SpinLock<MmioFreeRegionList>; MMIO_BUDDY_REGION_COUNT as usize],
}

/// 空闲内存块链表结构体
pub struct MmioFreeRegionList {
    // 存储了空闲内存块信息的结构体的链表
    list: LinkedList<Box<MmioBuddyAddrRegion>>,
    // 当前链表空闲块的数量
    num_free: i64,
}

/// mmio伙伴系统内部的地址区域结构体
pub struct MmioBuddyAddrRegion {
    // 内存块的起始地址
    vaddr: u64,
}                                  
                  
```

### 设计思路

&emsp;&emsp;DragonOS中，使用`MmioBuddyMemPool`结构体作为buddy（为表述方便，以下将伙伴算法简称为buddy）内存池的数据结构，其记录了内存池的起始地址（pool_start_addr）以及内存池中内存块的总大小（pool_size），同时其维护了大小为`MMIO_BUDDY_REGION_COUNT`的双向链表数组（free_regions）。`free_regions`中的各个链表维护了若干空闲内存块（MmioBuddyAddrRegion）。

&emsp;&emsp;`free_regions`的下标（index）与内存块的大小有关。由于每个内存块大小都为$2^{n}$ bit，那么可以令$exp = n$。index与exp的换算公式如下：$index = exp - 12$。e.g. 一个大小为$2^{12}$ bit的内存块，其$exp = 12$，使用上述公式计算得$index = 12 -12 = 0$，所以该内存块会被存入`free_regions[0].list`中。通过上述换算公式，每次取出或释放$2^n$大小的内存块，只需要操作`free_regions[n -12]`即可。DragonOS中，buddy内存池最大的内存块大小为$1G =  2^{30}bit$，最小的内存块大小为 $4K =  2^{12} bit$，所以$index\in[0,18]$。

&emsp;&emsp;作为内存分配机制，buddy服务于所有进程，为了解决在各个进程之间实现free_regions中的链表数据同步的问题，`free_regions`中的链表类型采用加了[自旋锁 &mdash; DragonOS dev 文档](https://docs.dragonos.org/zh_CN/latest/kernel/locking/spinlock.html#spinlock)（SpinLock）的空闲内存块链表（MmioFreeRegionList），`MmioFreeRegionList`中封装有真正的存储了空闲内存块信息的结构体的链表（list）和对应链表长度（num_free）。有了自选锁后，同一时刻只允许一个进程修改某个链表，如取出链表元素（申请内存）或者向链表中插入元素（释放内存）。

&emsp;&emsp;`MmioFreeRegionList`中的元素类型为`MmioBuddyAddrRegion`结构体，`MmioBuddyAddrRegion`记录了内存块的起始地址（vaddr）。

### 伙伴算法内部api

**P.S 以下函数均为MmioBuddyMemPool的成员函数。系统中已经创建了一个MmioBuddyMemPool类型的全局引用`MMIO_POOL`，如要使用以下函数，请以`MMIO_POOL.xxx()`形式使用，以此形式使用则不需要传入self。**

| **函数名**                                                           | **描述**                                                    |
|:----------------------------------------------------------------- |:--------------------------------------------------------- |
| __create_region(&self, vaddr)                                     | 将虚拟地址传入，创建新的内存块地址结构体                                      |
| __give_back_block(&self, vaddr, exp)                              | 将地址为vaddr，幂为exp的内存块归还给buddy                               |
| __buddy_split(&self,region,exp,list_guard)                        | 将给定大小为$2^{exp}$的内存块一分为二，并插入内存块大小为$2^{exp-1}$的链表中          |
| __query_addr_region(&self,exp,list_guard)                         | 从buddy中申请一块大小为$2^{exp}$的内存块                               |
| mmio_buddy_query_addr_region(&self,exp)                           | 对query_addr_region进行封装，**请使用这个函数，而不是__query_addr_region** |
| __buddy_add_region_obj(&self,region,list_guard)                   | 往指定的地址空间链表中添加一个内存块                                        |
| __buddy_block_vaddr(&self, vaddr, exp)                            | 根据地址和内存块大小，计算伙伴块虚拟内存的地址                                   |
| __pop_buddy_block( &self, vaddr,exp,list_guard)                   | 寻找并弹出指定内存块的伙伴块                                            |
| __buddy_pop_region( &self,        list_guard)                     | 从指定空闲链表中取出内存区域                                            |
| __buddy_merge(&self,exp,list_guard,high_list_guard)               | 合并所有$2^{exp}$大小的内存块                                       |
| __buddy_merge_blocks(&self,region_1,region_2,exp,high_list_guard) | 合并两个**已经从链表中取出的**内存块                                      |

### 伙伴算法对外api

| **函数名**                                         | **描述**                                      |
| ----------------------------------------------- | ------------------------------------------- |
| __mmio_buddy_init()                             | 初始化buddy系统，**在mmio_init()中调用，请勿随意调用**       |
| __exp2index(exp)                                | 将$2^{exp}$的exp转换成内存池中的数组的下标（index）          |
| mmio_create(size,vm_flags,res_vaddr,res_length) | 创建一块根据size对齐后的大小的mmio区域，并将其vma绑定到initial_mm |
| mmio_release(vaddr, length)                     | 取消地址为vaddr，大小为length的mmio的映射并将其归还到buddy中    |
