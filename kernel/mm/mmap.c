#include "mm.h"
#include "slab.h"
#include "internal.h"
#include <common/compiler.h>

extern uint64_t mm_total_2M_pages;

/**
 * @brief 虚拟地址长度所需要的entry数量
 *
 */
typedef struct
{
    int64_t num_PML4E;
    int64_t num_PDPTE;
    int64_t num_PDE;
    int64_t num_PTE;
} mm_pgt_entry_num_t;

/**
 * @brief 计算虚拟地址长度对应的页表entry数量
 *
 * @param length 长度
 * @param ent 返回的entry数量结构体
 */
static void mm_calculate_entry_num(uint64_t length, mm_pgt_entry_num_t *ent)
{
    if (ent == NULL)
        return;
    ent->num_PML4E = (length + (1UL << PAGE_GDT_SHIFT) - 1) >> PAGE_GDT_SHIFT;
    ent->num_PDPTE = (length + PAGE_1G_SIZE - 1) >> PAGE_1G_SHIFT;
    ent->num_PDE = (length + PAGE_2M_SIZE - 1) >> PAGE_2M_SHIFT;
    ent->num_PTE = (length + PAGE_4K_SIZE - 1) >> PAGE_4K_SHIFT;
}

/**
 * @brief 将物理地址映射到页表的函数
 *
 * @param virt_addr_start 要映射到的虚拟地址的起始位置
 * @param phys_addr_start 物理地址的起始位置
 * @param length 要映射的区域的长度（字节）
 * @param flags 标志位
 * @param use4k 是否使用4k页
 */
int mm_map_phys_addr(ul virt_addr_start, ul phys_addr_start, ul length, ul flags, bool use4k)
{
    uint64_t global_CR3 = (uint64_t)get_CR3();

    return mm_map_proc_page_table(global_CR3, true, virt_addr_start, phys_addr_start, length, flags, false, true, use4k);
}

int mm_map_phys_addr_user(ul virt_addr_start, ul phys_addr_start, ul length, ul flags)
{
    uint64_t global_CR3 = (uint64_t)get_CR3();
    return mm_map_proc_page_table(global_CR3, true, virt_addr_start, phys_addr_start, length, flags, true, true, false);
}

/**
 * @brief 将将物理地址填写到进程的页表的函数
 *
 * @param proc_page_table_addr 页表的基地址
 * @param is_phys 页表的基地址是否为物理地址
 * @param virt_addr_start 要映射到的虚拟地址的起始位置
 * @param phys_addr_start 物理地址的起始位置
 * @param length 要映射的区域的长度（字节）
 * @param user 用户态是否可访问
 * @param flush 是否刷新tlb
 * @param use4k 是否使用4k页
 */
int mm_map_proc_page_table(ul proc_page_table_addr, bool is_phys, ul virt_addr_start, ul phys_addr_start, ul length, ul flags, bool user, bool flush, bool use4k)
{

    // 计算线性地址对应的pml4页表项的地址
    mm_pgt_entry_num_t pgt_num;
    mm_calculate_entry_num(length, &pgt_num);

    // 已映射的内存大小
    uint64_t length_mapped = 0;

    // 对user标志位进行校正
    if (flags & PAGE_U_S)
        user = true;
    else
        user = false;

    uint64_t pml4e_id = ((virt_addr_start >> PAGE_GDT_SHIFT) & 0x1ff);
    uint64_t *pml4_ptr;
    if (is_phys)
        pml4_ptr = phys_2_virt((ul *)((ul)proc_page_table_addr & (~0xfffUL)));
    else
        pml4_ptr = (ul *)((ul)proc_page_table_addr & (~0xfffUL));

    // 循环填写顶层页表
    for (; (pgt_num.num_PML4E > 0) && pml4e_id < 512; ++pml4e_id)
    {
        // 剩余需要处理的pml4E -1
        --(pgt_num.num_PML4E);

        ul *pml4e_ptr = pml4_ptr + pml4e_id;

        // 创建新的二级页表
        if (*pml4e_ptr == 0)
        {
            ul *virt_addr = kmalloc(PAGE_4K_SIZE, 0);
            memset(virt_addr, 0, PAGE_4K_SIZE);
            set_pml4t(pml4e_ptr, mk_pml4t(virt_2_phys(virt_addr), (user ? PAGE_USER_PGT : PAGE_KERNEL_PGT)));
        }

        uint64_t pdpte_id = (((virt_addr_start + length_mapped) >> PAGE_1G_SHIFT) & 0x1ff);
        uint64_t *pdpt_ptr = (uint64_t *)phys_2_virt(*pml4e_ptr & (~0xfffUL));
        // kdebug("pdpt_ptr=%#018lx", pdpt_ptr);

        // 循环填写二级页表
        for (; (pgt_num.num_PDPTE > 0) && pdpte_id < 512; ++pdpte_id)
        {
            --pgt_num.num_PDPTE;
            uint64_t *pdpte_ptr = (pdpt_ptr + pdpte_id);
            // kdebug("pgt_num.num_PDPTE=%ld pdpte_ptr=%#018lx", pgt_num.num_PDPTE, pdpte_ptr);

            // 创建新的三级页表
            if (*pdpte_ptr == 0)
            {
                ul *virt_addr = kmalloc(PAGE_4K_SIZE, 0);
                memset(virt_addr, 0, PAGE_4K_SIZE);
                set_pdpt(pdpte_ptr, mk_pdpt(virt_2_phys(virt_addr), (user ? PAGE_USER_DIR : PAGE_KERNEL_DIR)));
                // kdebug("created new pdt, *pdpte_ptr=%#018lx, virt_addr=%#018lx", *pdpte_ptr, virt_addr);
            }

            uint64_t pde_id = (((virt_addr_start + length_mapped) >> PAGE_2M_SHIFT) & 0x1ff);
            uint64_t *pd_ptr = (uint64_t *)phys_2_virt(*pdpte_ptr & (~0xfffUL));
            // kdebug("pd_ptr=%#018lx, *pd_ptr=%#018lx", pd_ptr, *pd_ptr);

            // 循环填写三级页表，初始化2M物理页
            for (; (pgt_num.num_PDE > 0) && pde_id < 512; ++pde_id)
            {
                --pgt_num.num_PDE;
                // 计算当前2M物理页对应的pdt的页表项的物理地址
                ul *pde_ptr = pd_ptr + pde_id;

                // ====== 使用4k页 =======
                if (unlikely(use4k))
                {
                    // kdebug("use 4k");
                    if (*pde_ptr == 0)
                    {
                        // 创建四级页表
                        // kdebug("create PT");
                        uint64_t *vaddr = kmalloc(PAGE_4K_SIZE, 0);
                        memset(vaddr, 0, PAGE_4K_SIZE);
                        set_pdt(pde_ptr, mk_pdt(virt_2_phys(vaddr), (user ? PAGE_USER_PDE : PAGE_KERNEL_PDE)));
                    }
                    else if (unlikely(*pde_ptr & (1 << 7)))
                    {
                        // 当前页表项已经被映射了2MB物理页
                        goto failed;
                    }

                    uint64_t pte_id = (((virt_addr_start + length_mapped) >> PAGE_4K_SHIFT) & 0x1ff);
                    uint64_t *pt_ptr = (uint64_t *)phys_2_virt(*pde_ptr & (~0x1fffUL));

                    // 循环填写4级页表，初始化4K页
                    for (; pgt_num.num_PTE > 0 && pte_id < 512; ++pte_id)
                    {
                        --pgt_num.num_PTE;
                        uint64_t *pte_ptr = pt_ptr + pte_id;

                        if (unlikely(*pte_ptr != 0))
                        {
                            kwarn("pte already exists.");
                            length_mapped += PAGE_4K_SIZE;
                        }

                        set_pt(pte_ptr, mk_pt((ul)phys_addr_start + length_mapped, flags | (user ? PAGE_USER_4K_PAGE : PAGE_KERNEL_4K_PAGE)));
                    }
                }
                // ======= 使用2M页 ========
                else
                {
                    if (unlikely(*pde_ptr != 0 && user))
                    {
                        // 如果是用户态可访问的页，则释放当前新获取的物理页
                        if (likely((((ul)phys_addr_start + length_mapped) >> PAGE_2M_SHIFT) < mm_total_2M_pages)) // 校验是否为内存中的物理页
                            free_pages(Phy_to_2M_Page((ul)phys_addr_start + length_mapped), 1);
                        length_mapped += PAGE_2M_SIZE;
                        continue;
                    }
                    // 页面写穿，禁止缓存
                    set_pdt(pde_ptr, mk_pdt((ul)phys_addr_start + length_mapped, flags | (user ? PAGE_USER_PAGE : PAGE_KERNEL_PAGE)));
                    length_mapped += PAGE_2M_SIZE;
                }
            }
        }
    }
    if (likely(flush))
        flush_tlb();
    return 0;
failed:;
    kerror("Map memory failed. use4k=%d, vaddr=%#018lx, paddr=%#018lx", use4k, virt_addr_start, phys_addr_start);
    return -EFAULT;
}

/**
 * @brief 从页表中清除虚拟地址的映射
 *
 * @param proc_page_table_addr 页表的地址
 * @param is_phys 页表地址是否为物理地址
 * @param virt_addr_start 要清除的虚拟地址的起始地址
 * @param length 要清除的区域的长度
 */
void mm_unmap_proc_table(ul proc_page_table_addr, bool is_phys, ul virt_addr_start, ul length)
{

    // 计算线性地址对应的pml4页表项的地址
    mm_pgt_entry_num_t pgt_num;
    mm_calculate_entry_num(length, &pgt_num);
    // kdebug("ent1=%d ent2=%d ent3=%d, ent4=%d", pgt_num.num_PML4E, pgt_num.num_PDPTE, pgt_num.num_PDE, pgt_num.num_PTE);
    // 已取消映射的内存大小
    uint64_t length_unmapped = 0;

    uint64_t pml4e_id = ((virt_addr_start >> PAGE_GDT_SHIFT) & 0x1ff);
    uint64_t *pml4_ptr;
    if (is_phys)
        pml4_ptr = phys_2_virt((ul *)((ul)proc_page_table_addr & (~0xfffUL)));
    else
        pml4_ptr = (ul *)((ul)proc_page_table_addr & (~0xfffUL));

    // 循环填写顶层页表
    for (; (pgt_num.num_PML4E > 0) && pml4e_id < 512; ++pml4e_id)
    {
        // 剩余需要处理的pml4E -1
        --(pgt_num.num_PML4E);

        ul *pml4e_ptr = NULL;
        pml4e_ptr = pml4_ptr + pml4e_id;

        // 二级页表不存在
        if (*pml4e_ptr == 0)
        {
            continue;
        }

        uint64_t pdpte_id = (((virt_addr_start + length_unmapped) >> PAGE_1G_SHIFT) & 0x1ff);
        uint64_t *pdpt_ptr = (uint64_t *)phys_2_virt(*pml4e_ptr & (~0xfffUL));
        // kdebug("pdpt_ptr=%#018lx", pdpt_ptr);

        // 循环处理二级页表
        for (; (pgt_num.num_PDPTE > 0) && pdpte_id < 512; ++pdpte_id)
        {
            --pgt_num.num_PDPTE;
            uint64_t *pdpte_ptr = (pdpt_ptr + pdpte_id);
            // kdebug("pgt_num.num_PDPTE=%ld pdpte_ptr=%#018lx", pgt_num.num_PDPTE, pdpte_ptr);

            // 三级页表为空
            if (*pdpte_ptr == 0)
            {
                continue;
            }

            uint64_t pde_id = (((virt_addr_start + length_unmapped) >> PAGE_2M_SHIFT) & 0x1ff);
            uint64_t *pd_ptr = (uint64_t *)phys_2_virt(*pdpte_ptr & (~0xfffUL));
            // kdebug("pd_ptr=%#018lx, *pd_ptr=%#018lx", pd_ptr, *pd_ptr);

            // 循环处理三级页表
            for (; (pgt_num.num_PDE > 0) && pde_id < 512; ++pde_id)
            {
                --pgt_num.num_PDE;
                // 计算当前2M物理页对应的pdt的页表项的物理地址
                ul *pde_ptr = pd_ptr + pde_id;

                // 存在4级页表
                if (((*pde_ptr) & (1 << 7)) == 0)
                {
                    // 存在4K页
                    uint64_t pte_id = (((virt_addr_start + length_unmapped) >> PAGE_4K_SHIFT) & 0x1ff);
                    uint64_t *pt_ptr = (uint64_t *)phys_2_virt(*pde_ptr & (~0x1fffUL));
                    uint64_t *pte_ptr = pt_ptr + pte_id;

                    // 循环处理4K页表
                    for (; pgt_num.num_PTE > 0 && pte_id < 512; ++pte_id, ++pte_ptr)
                    {
                        --pgt_num.num_PTE;
                        // todo: 当支持使用slab分配4K内存作为进程的4K页之后，在这里需要释放这些4K对象
                        *pte_ptr = 0;
                        length_unmapped += PAGE_4K_SIZE;
                    }

                    // 4级页表已经空了，释放页表
                    if (unlikely(mm_check_page_table(pt_ptr)) == 0)
                        kfree(pt_ptr);
                }
                else
                {
                    *pde_ptr = 0;
                    length_unmapped += PAGE_2M_SIZE;
                    pgt_num.num_PTE -= 512;
                }
            }

            // 3级页表已经空了，释放页表
            if (unlikely(mm_check_page_table(pd_ptr)) == 0)
                kfree(pd_ptr);
        }
        // 2级页表已经空了，释放页表
        if (unlikely(mm_check_page_table(pdpt_ptr)) == 0)
            kfree(pdpt_ptr);
    }
    flush_tlb();
}

/**
 * @brief 创建VMA，并将物理地址映射到指定的虚拟地址处
 *
 * @param mm 要绑定的内存空间分布结构体
 * @param vaddr 起始虚拟地址
 * @param length 长度（字节）
 * @param paddr 起始物理地址
 * @param vm_flags vma的标志
 * @param vm_ops vma的操作接口
 * @return int 错误码
 */
int mm_map_vma(struct mm_struct *mm, uint64_t vaddr, uint64_t length, uint64_t paddr, vm_flags_t vm_flags, struct vm_operations_t *vm_ops)
{
    int retval = 0;
    struct vm_area_struct *vma = vm_area_alloc(mm);
    if (unlikely(vma == NULL))
        return -ENOMEM;
    vma->vm_ops = vm_ops;
    vma->vm_flags = vm_flags;
    vma->vm_start = vaddr;
    vma->vm_end = vaddr + length;

    // 将VMA加入链表
    __vma_link_list(mm, vma, mm->vmas);
    uint64_t len_4k = length % PAGE_2M_SIZE;
    uint64_t len_2m = length - len_4k;

    // ==== 将地址映射到页表
    /*
        todo: 限制页面的读写权限
    */

    // 先映射2M页
    if (likely(len_2m > 0))
    {
        uint64_t page_flags = 0;
        if (vm_flags & VM_USER)
            page_flags = PAGE_USER_PAGE;
        else
            page_flags = PAGE_KERNEL_PAGE;
        // 这里直接设置user标志位为false，因为该函数内部会对其进行自动校正
        retval = mm_map_proc_page_table((uint64_t)mm->pgd, true, vaddr, paddr, len_2m, page_flags, false, false, false);
        if (unlikely(retval != 0))
            goto failed;
    }

    if (likely(len_4k > 0))
    {
        len_4k = ALIGN(len_4k, PAGE_4K_SIZE);

        uint64_t page_flags = 0;
        if (vm_flags & VM_USER)
            page_flags = PAGE_USER_4K_PAGE;
        else
            page_flags = PAGE_KERNEL_4K_PAGE;
        // 这里直接设置user标志位为false，因为该函数内部会对其进行自动校正
        retval = mm_map_proc_page_table((uint64_t)mm->pgd, true, vaddr + len_2m, paddr + len_2m, len_4k, page_flags, false, false, true);
        if (unlikely(retval != 0))
            goto failed;
    }

    flush_tlb();
    return 0;
failed:;
    __vma_unlink_list(mm, vma);
    vm_area_free(vma);
    return retval;
}

/**
 * @brief 在页表中取消指定的vma的映射
 *
 * @param mm 指定的mm
 * @param vma 待取消映射的vma
 * @param paddr 返回的被取消映射的起始物理地址
 * @return int 返回码
 */
int mm_umap_vma(struct mm_struct *mm, struct vm_area_struct *vma, uint64_t *paddr)
{
    // 确保vma对应的mm与指定的mm相一致
    if (unlikely(vma->vm_mm != mm))
        return -EINVAL;

    if (paddr != NULL)
        *paddr = __mm_get_paddr(mm, vma->vm_start);

    mm_unmap_proc_table((uint64_t)mm->pgd, true, vma->vm_start, vma->vm_end - vma->vm_start);
    return 0;
}