#include "mm.h"
#include "slab.h"
#include "internal.h"

/**
 * @brief 获取一块新的vma结构体，并将其与指定的mm进行绑定
 *
 * @param mm 与VMA绑定的内存空间分布结构体
 * @return struct vm_area_struct* 新的VMA
 */
struct vm_area_struct *vm_area_alloc(struct mm_struct *mm)
{
    struct vm_area_struct *vma = (struct vm_area_struct *)kmalloc(sizeof(struct vm_area_struct), 0);
    if (vma)
        vma_init(vma, mm);
    return vma;
}

/**
 * @brief 从链表中删除指定的vma结构体
 *
 * @param vma
 */
void vm_area_del(struct vm_area_struct *vma)
{
    if (vma->vm_mm == NULL)
        return;
    __vma_unlink_list(vma->vm_mm, vma);
}

/**
 * @brief 释放vma结构体
 *
 * @param vma 待释放的vma结构体
 */
void vm_area_free(struct vm_area_struct *vma)
{
    if (vma->vm_prev == NULL && vma->vm_next == NULL) // 如果当前是剩余的最后一个vma
        vma->vm_mm->vmas = NULL;
    kfree(vma);
}

/**
 * @brief 将vma结构体插入mm_struct的链表之中
 *
 * @param mm 内存空间分布结构体
 * @param vma 待插入的VMA结构体
 * @param prev 链表的前一个结点
 */
void __vma_link_list(struct mm_struct *mm, struct vm_area_struct *vma, struct vm_area_struct *prev)
{
    struct vm_area_struct *next = NULL;
    vma->vm_prev = prev;
    if (prev) // 若指定了前一个结点，则直接连接
    {
        next = prev->vm_next;
        prev->vm_next = vma;
    }
    else // 否则将vma直接插入到给定的mm的vma链表之中
    {
        next = mm->vmas;
        mm->vmas = vma;
    }

    vma->vm_next = next;

    if (next != NULL)
        next->vm_prev = vma;
}

/**
 * @brief 将vma给定结构体从vma链表的结点之中删除
 *
 * @param mm 内存空间分布结构体
 * @param vma 待插入的VMA结构体
 */
void __vma_unlink_list(struct mm_struct *mm, struct vm_area_struct *vma)
{
    struct vm_area_struct *prev, *next;
    next = vma->vm_next;
    prev = vma->vm_prev;
    if (prev)
        prev->vm_next = next;
    else // 当前vma是链表中的第一个vma
        mm->vmas = next;

    if (next)
        next->vm_prev = prev;
}

/**
 * @brief 查找第一个符合“addr < vm_end”条件的vma
 *
 * @param mm 内存空间分布结构体
 * @param addr 虚拟地址
 * @return struct vm_area_struct* 符合条件的vma
 */
struct vm_area_struct *vma_find(struct mm_struct *mm, uint64_t addr)
{
    struct vm_area_struct *vma = mm->vmas;
    struct vm_area_struct *result = NULL;
    while (vma != NULL)
    {
        if (vma->vm_end > addr)
        {
            result = vma;
            break;
        }
        vma = vma->vm_next;
    }
    return result;
}

/**
 * @brief 插入vma
 *
 * @param mm
 * @param vma
 * @return int
 */
int vma_insert(struct mm_struct *mm, struct vm_area_struct *vma)
{

    struct vm_area_struct *prev;

    prev = vma_find(mm, vma->vm_start);
    
    if (prev && prev->vm_start <= vma->vm_start && prev->vm_end >= vma->vm_end)
    {
        // 已经存在了相同的vma
        return -EEXIST;
    }
    // todo: bugfix: 这里的第二种情况貌似从来不会满足
    else if (prev && ((vma->vm_start >= prev->vm_start && vma->vm_start <= prev->vm_end) || (prev->vm_start <= vma->vm_end && prev->vm_start >= vma->vm_start)))    
    {
        //部分重叠
        if ((!CROSS_2M_BOUND(vma->vm_start, prev->vm_start)) && (!CROSS_2M_BOUND(vma->vm_end, prev->vm_end))&& vma->vm_end)
        {
            //合并vma 并改变链表vma的范围
            kdebug("before combining vma:vm_start = %#018lx, vm_end = %#018lx\n", vma->vm_start, vma->vm_end);

            prev->vm_start = (vma->vm_start < prev->vm_start )? vma->vm_start : prev->vm_start;
            prev->vm_end = (vma->vm_end > prev->vm_end) ? vma->vm_end : prev->vm_end;
            // 计算page_offset
            prev->page_offset = prev->vm_start - (prev->vm_start & PAGE_2M_MASK);
            kdebug("combined vma:vm_start = %#018lx, vm_end = %#018lx\nprev:vm_start = %018lx, vm_end = %018lx\n", vma->vm_start, vma->vm_end, prev->vm_start, prev->vm_end);
            kinfo("vma has same part\n");
            return __VMA_MERGED;
        }
    }

    // prev = vma_find(mm, vma->vm_start);

    if (prev == NULL) // 要将当前vma插入到链表的尾部
    {
        struct vm_area_struct *ptr = mm->vmas;
        while (ptr)
        {
            if (ptr->vm_next)
                ptr = ptr->vm_next;
            else
            {
                prev = ptr;
                break;
            }
        }
    }
    else
        prev = prev->vm_prev;
    __vma_link_list(mm, vma, prev);
    return 0;
}

/**
 * @brief 创建anon_vma，并将其与页面结构体进行绑定
 * 若提供的页面结构体指针为NULL，则只创建，不绑定
 *
 * @param page 页面结构体的指针
 * @param lock_page 是否将页面结构体加锁
 * @return struct anon_vma_t* 创建好的anon_vma
 */
struct anon_vma_t *__anon_vma_create_alloc(struct Page *page, bool lock_page)
{
    struct anon_vma_t *anon_vma = (struct anon_vma_t *)kmalloc(sizeof(struct anon_vma_t), 0);
    if (unlikely(anon_vma == NULL))
        return NULL;
    memset(anon_vma, 0, sizeof(struct anon_vma_t));

    list_init(&anon_vma->vma_list);
    semaphore_init(&anon_vma->sem, 1);

    // 需要和page进行绑定
    if (page != NULL)
    {
        if (lock_page == true) // 需要加锁
        {
            uint64_t rflags;
            spin_lock(&page->op_lock);
            page->anon_vma = anon_vma;
            spin_unlock(&page->op_lock);
        }
        else
            page->anon_vma = anon_vma;

        anon_vma->page = page;
    }
    return anon_vma;
}

/**
 * @brief 将指定的vma加入到anon_vma的管理范围之中
 *
 * @param anon_vma 页面的anon_vma
 * @param vma 待加入的vma
 * @return int 返回码
 */
int __anon_vma_add(struct anon_vma_t *anon_vma, struct vm_area_struct *vma)
{
    semaphore_down(&anon_vma->sem);
    list_add(&anon_vma->vma_list, &vma->anon_vma_list);
    vma->anon_vma = anon_vma;
    atomic_inc(&anon_vma->ref_count);
    semaphore_up(&anon_vma->sem);
    return 0;
}

/**
 * @brief 释放anon vma结构体
 *
 * @param anon_vma 待释放的anon_vma结构体
 * @return int 返回码
 */
int __anon_vma_free(struct anon_vma_t *anon_vma)
{
    if (anon_vma->page != NULL)
    {
        spin_lock(&anon_vma->page->op_lock);
        anon_vma->page->anon_vma = NULL;
        spin_unlock(&anon_vma->page->op_lock);
    }
    kfree(anon_vma);

    return 0;
}

/**
 * @brief 从anon_vma的管理范围中删除指定的vma
 * (在进入这个函数之前，应该要对anon_vma加锁)
 * @param vma 将要取消对应的anon_vma管理的vma结构体
 * @return int 返回码
 */
int __anon_vma_del(struct vm_area_struct *vma)
{
    // 当前vma没有绑定anon_vma
    if (vma->anon_vma == NULL)
        return -EINVAL;

    list_del(&vma->anon_vma_list);
    atomic_dec(&vma->anon_vma->ref_count);

    // 若当前anon_vma的引用计数归零，则意味着可以释放内存页
    if (unlikely(atomic_read(&vma->anon_vma->ref_count) == 0)) // 应当释放该anon_vma
    {
        // 若页面结构体是mmio创建的，则释放页面结构体
        if (vma->anon_vma->page->attr & PAGE_DEVICE)
            kfree(vma->anon_vma->page);
        else
            free_pages(vma->anon_vma->page, 1);
        __anon_vma_free(vma->anon_vma);
    }

    // 清理当前vma的关联数据
    vma->anon_vma = NULL;
    list_init(&vma->anon_vma_list);
}
