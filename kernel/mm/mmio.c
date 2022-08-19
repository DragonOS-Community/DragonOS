#include "mmio.h"
#include "mmio-buddy.h"
#include <common/math.h>

void mmio_init()
{
    mmio_buddy_init();
}

/**
 * @brief 创建一块mmio区域，并将vma绑定到initial_mm
 *
 * @param size mmio区域的大小（字节）
 * @param vm_flags 要把vma设置成的标志
 * @param res_vaddr 返回值-分配得到的虚拟地址
 * @param res_length 返回值-分配的虚拟地址空间长度
 * @return int 错误码
 */
int mmio_create(uint32_t size, vm_flags_t vm_flags, uint64_t *res_vaddr, uint64_t *res_size)
{
    int retval = 0;
    // 申请的内存超过允许的最大大小
    if (unlikely(size > PAGE_1G_SIZE || size == 0))
        return -EPERM;

    // 计算要从buddy中申请地址空间大小(按照2的n次幂来对齐)
    int size_exp = 31 - __clz(size);
    if (size_exp < PAGE_4K_SHIFT)
    {
        size_exp = PAGE_4K_SHIFT;
        size = PAGE_4K_SIZE;
    }
    else if (size & (~(1 << size_exp)))
    {
        ++size_exp;
        size = 1 << size_exp;
    }
    // 申请内存
    struct __mmio_buddy_addr_region *buddy_region = mmio_buddy_query_addr_region(size_exp);
    if (buddy_region == NULL) // 没有空闲的mmio空间了
        return -ENOMEM;

    *res_vaddr = buddy_region->vaddr;
    *res_size = size;
    // 释放region
    __mmio_buddy_release_addr_region(buddy_region);

    // ====创建vma===
    // 设置vma flags
    vm_flags |= (VM_IO | VM_DONTCOPY);
    uint64_t len_4k = size % PAGE_2M_SIZE;
    uint64_t len_2m = size - len_4k;
    // 先创建2M的vma，然后创建4k的
    for (uint32_t i = 0; i < len_2m; i += PAGE_2M_SIZE)
    {

        retval = mm_create_vma(&initial_mm, buddy_region->vaddr + i, PAGE_2M_SIZE, vm_flags, NULL, NULL);
        if (unlikely(retval != 0))
            goto failed;
    }

    for (uint32_t i = len_2m; i < size; i += PAGE_4K_SIZE)
    {
        retval = mm_create_vma(&initial_mm, buddy_region->vaddr + i, PAGE_4K_SIZE, vm_flags, NULL, NULL);
        if (unlikely(retval != 0))
            goto failed;
    }
    return 0;
failed:;
    kerror("failed to create mmio vma. pid=%d", current_pcb->pid);
    // todo: 当失败时，将已创建的vma删除
    return retval;
}

/**
 * @brief 取消mmio的映射并将地址空间归还到buddy中
 *
 * @param vaddr 起始的虚拟地址
 * @param length 要归还的地址空间的长度
 * @return int 错误码
 */
int mmio_release(uint64_t vaddr, uint64_t length)
{
    int retval = 0;
    // 先将这些区域都unmap了
    mm_unmap(&initial_mm, vaddr, length, false);

    // 将这些区域加入buddy
    for (uint64_t i = 0; i < length;)
    {
        struct vm_area_struct *vma = vma_find(&initial_mm, vaddr + i);
        if (unlikely(vma == NULL))
        {
            kerror("mmio_release failed: vma not found. At address: %#018lx, pid=%ld", vaddr + i, current_pcb->pid);
            return -EINVAL;
        }

        if (unlikely(vma->vm_start != (vaddr + i)))
        {
            kerror("mmio_release failed: addr_start is not equal to current: %#018lx.", vaddr + i);
            return -EINVAL;
        }
        // 往buddy中插入内存块
        retval = __mmio_buddy_give_back(vma->vm_start, 31 - __clz(vma->vm_end - vma->vm_start));
        i += vma->vm_end - vma->vm_start;

        // 释放vma结构体
        vm_area_del(vma);
        vm_area_free(vma);

        if (unlikely(retval != 0))
            goto give_back_failed;
    }
    return 0;
give_back_failed:;
    kerror("mmio_release give_back failed: ");
    return retval;
}
