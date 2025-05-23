:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/memory_management/intro.md

- Translation time: 2025-05-19 01:41:11

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Introduction to the Memory Management Module

## 1. Overview

&emsp;&emsp;DragonOS implements a memory management module with excellent architectural design, encapsulating operations such as memory mapping, allocation, release, and management for both kernel space and user space. This allows kernel developers to more conveniently perform memory management tasks.

&emsp;&emsp;The memory management module of DragonOS is mainly composed of the following types of components:

- **Hardware Abstraction Layer (MemoryManagementArch)** - Provides abstraction for specific processor architectures, enabling the memory management module to run on different processor architectures.
- **Page Mapper (PageMapper)** - Provides mapping between virtual addresses and physical addresses, as well as operations for creating, filling, destroying, and managing page tables. It is divided into two types: Kernel Page Table Mapper (KernelMapper) and User Page Table Mapper (located within the specific user address space structure).
- **Page Flusher (PageFlusher)** - Provides operations for flushing page tables (full table flush, single page flush, cross-core flush).
- **Frame Allocator (FrameAllocator)** - Provides operations for allocating, releasing, and managing page frames. Specifically, it includes BumpAllocator and BuddyAllocator.
- **Small Object Allocator** - Provides operations for allocating, releasing, and managing small memory objects. This refers to the SlabAllocator within the kernel (the implementation of SlabAllocator is currently not completed).
- **MMIO Space Manager** - Provides operations for allocating and managing MMIO address spaces. (This module is pending further refactoring.)
- **User Address Space Management Mechanism** - Provides management of user address spaces.
    - **VMA Mechanism** - Provides management of user address spaces, including creation, destruction, and permission management of VMA.
    - **User Mapping Management** - Works together with the VMA mechanism to manage mappings in user address spaces.
- **System Call Layer** - Provides system calls for the user-space memory management system, including mmap, munmap, mprotect, mremap, etc.
- **C Interface Compatibility Layer** - Provides interfaces for existing C code, enabling C code to run normally.
