# 内存分配指南

&emsp;&emsp;本文将讲述如何在内核中进行内存分配。在开始之前，请您先了解一个基本点：DragonOS的内核使用4KB的页来管理内存，并且具有伙伴分配器和slab分配器。并且对用户空间、内核空间均具有特定的管理机制。

## 1. 安全的分配内存

&emsp;&emsp;在默认情况下，KernelAllocator被绑定为全局内存分配器，它会根据请求分配的内存大小，自动选择使用slab还是伙伴分配器。因此，在内核中，使用Rust原生的
内存分配函数，或者是创建一个`Box`对象等等，都是安全的。


## 2. 手动管理页帧

:::{warning}
**请格外小心！** 手动管理页帧脱离了Rust的内存安全机制，因此可能会造成内存泄漏或者是内存错误。
:::

&emsp;&emsp;在某些情况下，我们需要手动分配页帧。例如，我们需要在内核中创建一个新的页表，或者是在内核中创建一个新的地址空间。这时候，我们需要手动分配页帧。使用`LockedFrameAllocator`的`allocate()`函数，能够分配在物理地址上连续的页帧。请注意，由于底层使用的是buddy分配器，因此页帧数目必须是2的n次幂，且最大大小不超过1GB。

&emsp;&emsp;当需要释放页帧的时候，使用`LockedFrameAllocator`的`deallocate()`函数，或者是`deallocate_page_frames()`函数，能够释放在物理地址上连续的页帧。

&emsp;&emsp;当您需要映射页帧的时候，可使用`KernelMapper::lock()`函数，获得一个内核映射器对象，然后进行映射。由于KernelMapper是对PageMapper的封装，因此您在获取KernelMapper之后，可以使用PageMapper相关接口对内核空间的映射进行管理。

:::{warning}
**千万不要** 使用KernelMapper去映射用户地址空间的内存，这会使得这部分内存脱离用户地址空间的管理，从而导致内存错误。
:::

## 3. 为用户程序分配内存

&emsp;&emsp;在内核中，您可以使用用户地址空间结构体(`AddressSpace`)的`mmap()`,`map_anonymous()`等函数，为用户程序分配内存。这些函数会自动将用户程序的内存映射到用户地址空间中，并且会自动创建VMA结构体。您可以使用`AddressSpace`的`munmap()`函数，将用户程序的内存从用户地址空间中解除映射，并且销毁VMA结构体。调整权限等操作可以使用`AddressSpace`的`mprotect()`函数。
