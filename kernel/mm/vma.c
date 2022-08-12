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
    if (prev && prev->vm_start == vma->vm_start && prev->vm_end == vma->vm_end)
    {
        // 已经存在了相同的vma
        return -EEXIST;
    }
    else if (prev && (prev->vm_start == vma->vm_start || prev->vm_end == vma->vm_end)) // 暂时不支持扩展vma
    {
        kwarn("Not support: expand vma");
        return -ENOTSUP;
    }

    prev = vma_find(mm, vma->vm_end);
    if (prev)
        prev = prev->vm_prev;
    if (prev == NULL) // 要将当前vma插入到链表的尾部
    {
        struct vm_area_struct * ptr = mm->vmas;
        while(ptr)
        {
            if(ptr->vm_next)
                ptr = ptr->vm_next;
            else
            {
                prev = ptr;
                break;
            }
        }
    }
    __vma_link_list(mm, vma, prev);
    return 0;
}