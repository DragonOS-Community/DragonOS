#include "mm.h"
#include "slab.h"

/**
 * @brief 获取一块新的vma结构体，并将其与指定的mm进行绑定
 * 
 * @param mm 与VMA绑定的内存空间分布结构体
 * @return struct vm_area_struct* 新的VMA
 */
struct vm_area_struct * vm_area_alloc(struct mm_struct *mm)
{
    struct vm_area_struct * vma = (struct vm_area_struct *)kmalloc(sizeof(struct vm_area_struct),0);
    if(vma)
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
    if(list_empty(&vma->list))  // 如果当前是剩余的最后一个vma
        vma->vm_mm->vmas = NULL;
    kfree(vma);
}