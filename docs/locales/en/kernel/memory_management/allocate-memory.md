:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/memory_management/allocate-memory.md

- Translation time: 2025-05-19 01:41:13

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Memory Allocation Guide

&emsp;&emsp;This document will explain how to perform memory allocation within the kernel. Before starting, please understand a basic point: DragonOS's kernel manages memory using 4KB pages and has a buddy allocator and a slab allocator. It also has specific management mechanisms for both user space and kernel space.

## 1. Safe Memory Allocation

&emsp;&emsp;By default, KernelAllocator is bound as the global memory allocator. It automatically selects between using the slab allocator or the buddy allocator based on the size of the memory requested. Therefore, in the kernel, using Rust's native memory allocation functions or creating an `Box` object, etc., is safe.

## 2. Manual Management of Page Frames

:::{warning}
**Please be extremely cautious!** Manually managing page frames bypasses Rust's memory safety mechanisms, which may lead to memory leaks or memory errors.
:::

&emsp;&emsp;In some cases, we need to manually allocate page frames. For example, when we need to create a new page table or a new address space within the kernel. In such situations, we need to manually allocate page frames. Using the `LockedFrameAllocator`'s `allocate()` function can allocate contiguous page frames in physical address space. Please note that since the underlying implementation uses the buddy allocator, the number of page frames must be a power of two, and the maximum size should not exceed 1GB.

&emsp;&emsp;When you need to release page frames, you can use the `LockedFrameAllocator`'s `deallocate()` function or the `deallocate_page_frames()` function to release contiguous page frames in physical address space.

&emsp;&emsp;When you need to map page frames, you can use the `KernelMapper::lock()` function to obtain a kernel mapper object and then perform the mapping. Since KernelMapper is an encapsulation of PageMapper, once you obtain a KernelMapper, you can use the PageMapper-related interfaces to manage the mapping in the kernel space.

:::{warning}
**Never** use KernelMapper to map memory in user address space. This would cause that part of memory to be detached from the user address space management, leading to memory errors.
:::

## 3. Allocating Memory for User Programs

&emsp;&emsp;In the kernel, you can use the user address space structure (`AddressSpace`) and its functions such as `mmap()`, `map_anonymous()`, etc., to allocate memory for user programs. These functions will automatically map the user program's memory into the user address space and automatically create VMA structures. You can use the `AddressSpace`'s `munmap()` function to unmap the user program's memory from the user address space and destroy the VMA structure. Functions such as `mprotect()` can be used for adjusting permissions.
