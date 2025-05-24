:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/memory_management/mmio.md

- Translation time: 2025-05-19 01:41:34

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# MMIO

MMIO stands for "Memory-Mapped I/O," and it is widely used for interaction with hardware devices.

## Address Space Management

DragonOS implements a mechanism for managing MMIO address spaces. This section will introduce them.

### Why is Automatic Allocation of MMIO Address Space Needed?

&emsp;&emsp;Since many devices on a computer require MMIO address space, and the demand for MMIO address space varies among different devices connected to each computer, manually specifying an MMIO address for each device type would lead to significant waste of virtual address space and increase system complexity. Moreover, we will need to handle exception handling functions for different virtual memory regions in the future. Therefore, we need a mechanism that can automatically allocate MMIO address space.

### What Features Does This Mechanism Provide?

- Allocates MMIO virtual address space ranging from 4K to 1GB for drivers
- Adds these virtual address spaces to VMA for unified management
- Allows batch release of these address spaces

### How Is This Mechanism Implemented?

&emsp;&emsp;This mechanism essentially uses the buddy system to maintain MMIO virtual address space. In `mm/mm.h`, the MMIO virtual address space range is specified, which starts at `0xffffa10000000000` and covers a 1TB space. In other words, the buddy system maintains this 1TB virtual address space for MMIO.

### Address Space Allocation Process

1. Initialize the MMIO-mapping module, creating 512 1GB `__mmio_buddy_addr_region` blocks in the MMIO buddy system.
2. The driver uses `mmio_create` to request address space allocation.
3. `mmio_create` aligns the requested address space size to the nearest power of 2 and allocates memory address space from the buddy.
4. Create a VMA and mark it as `VM_IO|VM_DONTCOPY`. MMIO VMA is only bound under `initial_mm` and will not be copied.
5. Allocation is complete.

Once the MMIO address space is allocated, it behaves like a regular VMA and can be operated using mmap series functions.

### MMIO Mapping Process

&emsp;&emsp;After obtaining the virtual address space, when we attempt to map memory into this address space, we can call the `mm_map` function to map this region.

&emsp;&emsp;This function performs special handling for MMIO VMA mapping. That is: it creates a `Page` structure and the corresponding `anon_vma`. Then, it fills the corresponding physical address into the page table.

### Releasing MMIO Virtual Address Space

&emsp;&emsp;When a device is unmounted, the driver can call the `mmio_release` function to release the specified MMIO address space.

&emsp;&emsp;During the release process, `mmio_release` performs the following steps:

1. Remove the MMIO region's mapping from the page table.
2. Release the MMIO region's VMA.
3. Return the address space back to the MMIO buddy system.

## Buddy Algorithm for MMIO

### Definition of Buddy

&emsp;&emsp;Two memory blocks are considered buddy memory blocks if they satisfy the following three conditions:

1. The sizes of the two memory blocks are the same.
2. The memory addresses of the two memory blocks are contiguous.
3. The two memory blocks are derived from the same larger block of memory.

### Buddy Algorithm

&emsp;&emsp;The buddy algorithm is used to manage and organize the allocation and recycling of large contiguous memory blocks to reduce external fragmentation during system runtime. In the buddy system, each memory block size is $2^n$ bytes. In DragonOS, the buddy system memory pool maintains a total of 1TB of contiguous storage space, with the largest memory block size being $1G$ (i.e., $2^{30}B$) and the smallest memory block size being $4K$ (i.e., $2^{12}B$).

&emsp;&emsp;The core idea of the buddy algorithm is that when an application requests memory, it always allocates the smallest memory block larger than the requested size, and the allocated memory block size is $2^nB$. (e.g., if an application requests $3B$ of memory, there is no integer $n$ such that $2^n = 3$, and $3 \in [2^1, 2^2]$, so the system will allocate a $2^2B$ memory block to the application, and the memory request is successfully completed.)

&emsp;&emsp;What if there is no such "suitable" memory block in the buddy system? The system will first look for a larger memory block. If found, it will split the larger memory block into suitable memory blocks to allocate to the application. (e.g., if an application requests $3B$ of memory, and the system's smallest memory block larger than $3B$ is $16B$, then the $16B$ block will be split into two $8B$ blocks. One is placed back into the memory pool, and the other is further split into two $4B$ blocks. One of the $4B$ blocks is placed back into the memory pool, and the other is allocated to the application. Thus, the memory request is successfully completed.)

&emsp;&emsp;If the system cannot find a larger memory block, it will attempt to merge smaller memory blocks until the required size is met. (e.g., if an application requests $3B$ of memory, and the system checks the memory pool and finds only two $2B$ memory blocks, the system will merge these two $2B$ blocks into a $4B$ block and allocate it to the application. Thus, the memory request is successfully completed.)

&emsp;&emsp;Finally, if the system cannot find a large enough memory block and is unable to successfully merge smaller blocks, it will notify the application that there is not enough memory to allocate.

### Data Structures of the Buddy Algorithm

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

/// 最大的内存块为1G，其幂为30
const MMIO_BUDDY_MAX_EXP: u32 = PAGE_1G_SHIFT;
/// 最小的内存块为4K，其幂为12                                  
const MMIO_BUDDY_MIN_EXP: u32 = PAGE_4K_SHIFT;
/// 内存池数组的大小为18
const MMIO_BUDDY_REGION_COUNT: u32 = MMIO_BUDDY_MAX_EXP - MMIO_BUDDY_MIN_EXP + 1;

/// buddy内存池
pub struct MmioBuddyMemPool {
    /// 内存池的起始地址
    pool_start_addr: u64, 
    /// 内存池大小：初始化为1TB
    pool_size: u64,
    /// 空闲内存块链表数组
    /// MMIO_BUDDY_REGION_COUNT = MMIO_BUDDY_MAX_EXP - MMIO_BUDDY_MIN_EXP + 1
    free_regions: [SpinLock<MmioFreeRegionList>; MMIO_BUDDY_REGION_COUNT as usize],
}

/// 空闲内存块链表结构体
pub struct MmioFreeRegionList {
    /// 存储了空闲内存块信息的结构体的链表
    list: LinkedList<Box<MmioBuddyAddrRegion>>,
    /// 当前链表空闲块的数量
    num_free: i64,
}

/// mmio伙伴系统内部的地址区域结构体
pub struct MmioBuddyAddrRegion {
    /// 内存块的起始地址
    vaddr: u64,
}                                  
                  
```

### Design Philosophy

&emsp;&emsp;In DragonOS, the `MmioBuddyMemPool` structure is used as the data structure for the buddy (for convenience of expression, the buddy algorithm is referred to as buddy) memory pool. It records the starting address (pool_start_addr) of the memory pool and the total size of memory blocks in the memory pool (pool_size). It also maintains a bidirectional linked list array (free_regions) of size `MMIO_BUDDY_REGION_COUNT`. Each linked list in `free_regions` maintains several free memory blocks (MmioBuddyAddrRegion).

&emsp;&emsp;The index of `free_regions` is related to the size of the memory block. Since each memory block size is $2^{n}$ bytes, we can let $exp = n$. The conversion formula between index and exp is as follows: $index = exp - 12$. For example, a memory block of size $2^{12}$ bytes has $exp = 12$, and using the above formula, we get $index = 12 - 12 = 0$, so this memory block is stored in `free_regions[0].list`. Through this conversion formula, each time we take or release a memory block of size $2^n$, we only need to operate on `free_regions[n -12]`. In DragonOS, the largest memory block size in the buddy memory pool is $1G = 2^{30} bytes$, and the smallest is $4K = 2^{12} bytes$, so $index \in [0, 18]$.

&emsp;&emsp;As a memory allocation mechanism, the buddy serves all processes. To solve the problem of synchronizing the linked list data in free_regions among different processes, the linked list type in `free_regions` uses a SpinLock (denoted as {ref}`自旋锁 <_spinlock_doc_spinlock>`) to protect the free memory block linked list (MmioFreeRegionList). `MmioFreeRegionList` encapsulates a linked list (list) that actually stores information about free memory blocks and the corresponding list length (num_free). With the use of a spinlock, only one process can modify a particular linked list at the same time, such as taking elements from the list (memory allocation) or inserting elements into the list (memory release).

&emsp;&emsp;The element type in `MmioFreeRegionList` is the `MmioBuddyAddrRegion` structure, which records the starting address (vaddr) of the memory block.

### Internal APIs of the Buddy Algorithm

**P.S. The following functions are all members of the MmioBuddyMemPool structure. A global reference of type MmioBuddyMemPool has already been created in the system, denoted as `MMIO_POOL`. To use the following functions, please use them in the form of `MMIO_POOL.xxx()`, which does not require passing self.**

| **Function Name**                                                           | **Description**                                                    |
|:----------------------------------------------------------------- |:--------------------------------------------------------- |
| __create_region(&self, vaddr)                                     | Pass the virtual address to create a new memory block address structure                                      |
| __give_back_block(&self, vaddr, exp)                              | Return the memory block at address vaddr with exponent exp back to the buddy                               |
| __buddy_split(&self, region, exp, list_guard)                        | Split the given memory block of size $2^{exp}$ into two and insert the memory blocks of size $2^{exp-1}$ into the list |
| __query_addr_region(&self, exp, list_guard)                         | Request a memory block of size $2^{exp}$ from the buddy                               |
| mmio_buddy_query_addr_region(&self, exp)                           | A wrapper for query_addr_region, **please use this function instead of __query_addr_region** |
| __buddy_add_region_obj(&self, region, list_guard)                   | Add a memory block to the specified address space list                                        |
| __buddy_block_vaddr(&self, vaddr, exp)                            | Calculate the virtual memory address of the buddy block based on the address and memory block size                                   |
| __pop_buddy_block( &self, vaddr, exp, list_guard)                   | Find and pop the buddy block of the specified memory block                                            |
| __buddy_pop_region( &self, list_guard)                     | Retrieve a memory region from the specified free list                                            |
| __buddy_merge(&self, exp, list_guard, high_list_guard)               | Merge all memory blocks of size $2^{exp}$                                       |
| __buddy_merge_blocks(&self, region_1, region_2, exp, high_list_guard) | Merge two **already removed from the list** memory blocks                                      |

### External APIs of the Buddy Algorithm

| **Function Name**                                         | **Description**                                      |
| ----------------------------------------------- | ------------------------------------------- |
| __mmio_buddy_init()                             | Initialize the buddy system, **called in mmio_init(), do not call it arbitrarily**       |
| __exp2index(exp)                                | Convert the exponent $exp$ of $2^{exp}$ to the index in the memory pool's array          |
| mmio_create(size, vm_flags, res_vaddr, res_length) | Create an MMIO region with size aligned to the requested size, and bind its VMA to initial_mm |
| mmio_release(vaddr, length)                     | Cancel the mapping of MMIO at address vaddr with size length and return it to the buddy    |
