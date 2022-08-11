#include "internal.h"

extern uint64_t mm_total_2M_pages;

/**
 * @brief 获取指定虚拟地址处映射的物理地址
 *
 * @param mm 内存空间分布结构体
 * @param vaddr 虚拟地址
 * @return uint64_t 已映射的物理地址
 */
uint64_t __mm_get_paddr(struct mm_struct *mm, uint64_t vaddr)
{
    ul *tmp;

    tmp = phys_2_virt((ul *)(((ul)mm->pgd) & (~0xfffUL)) + ((vaddr >> PAGE_GDT_SHIFT) & 0x1ff));

    // pml4页表项为0
    if (*tmp == 0)
        return 0;

    tmp = phys_2_virt((ul *)(*tmp & (~0xfffUL)) + ((vaddr >> PAGE_1G_SHIFT) & 0x1ff));

    // pdpt页表项为0
    if (*tmp == 0)
        return 0;

    // 读取pdt页表项
    tmp = phys_2_virt(((ul *)(*tmp & (~0xfffUL)) + (((ul)(vaddr) >> PAGE_2M_SHIFT) & 0x1ff)));

    // pde页表项为0
    if (*tmp == 0)
        return 0;

    if (*tmp & (1 << 7))
    {
        // 当前为2M物理页
        return (*tmp) & (~0x1fffUL);
    }
    else
    {
        // 存在4级页表
        tmp = phys_2_virt(((ul *)(*tmp & (~0xfffUL)) + (((ul)(vaddr) >> PAGE_4K_SHIFT) & 0x1ff)));

        return (*tmp) & (~0x1ffUL);
    }
}

/**
 * @brief 检测指定地址是否已经被映射
 *
 * @param page_table_phys_addr 页表的物理地址
 * @param virt_addr 要检测的地址
 * @return true 已经被映射
 * @return false
 */
bool mm_check_mapped(ul page_table_phys_addr, uint64_t virt_addr)
{
    ul *tmp;

    tmp = phys_2_virt((ul *)((ul)page_table_phys_addr & (~0xfffUL)) + ((virt_addr >> PAGE_GDT_SHIFT) & 0x1ff));

    // pml4页表项为0
    if (*tmp == 0)
        return 0;

    tmp = phys_2_virt((ul *)(*tmp & (~0xfffUL)) + ((virt_addr >> PAGE_1G_SHIFT) & 0x1ff));

    // pdpt页表项为0
    if (*tmp == 0)
        return 0;

    // 读取pdt页表项
    tmp = phys_2_virt(((ul *)(*tmp & (~0xfffUL)) + (((ul)(virt_addr) >> PAGE_2M_SHIFT) & 0x1ff)));

    // pde页表项为0
    if (*tmp == 0)
        return 0;

    if (*tmp & (1 << 7))
    {
        // 当前为2M物理页
        return true;
    }
    else
    {
        // 存在4级页表
        tmp = phys_2_virt(((ul *)(*tmp & (~0xfffUL)) + (((ul)(virt_addr) >> PAGE_4K_SHIFT) & 0x1ff)));
        if (*tmp != 0)
            return true;
        else
            return false;
    }
}

/**
 * @brief 检测是否为有效的2M页(物理内存页)
 *
 * @param paddr 物理地址
 * @return int8_t 是 -> 1
 *                 否 -> 0
 */
int8_t mm_is_2M_page(uint64_t paddr)
{
    if (likely((paddr >> PAGE_2M_SHIFT) < mm_total_2M_pages))
        return 1;
    else
        return 0;
}
