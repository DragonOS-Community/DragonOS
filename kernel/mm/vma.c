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