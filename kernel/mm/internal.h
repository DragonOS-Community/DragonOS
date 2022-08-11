#pragma once

#include "mm.h"

/**
 * @brief 将vma结构体插入mm_struct的链表之中
 * 
 * @param mm 内存空间分布结构体
 * @param vma 待插入的VMA结构体
 * @param prev 链表的前一个结点
 */
void __vma_link_list(struct mm_struct * mm, struct vm_area_struct * vma, struct vm_area_struct * prev);

/**
 * @brief 将vma给定结构体从vma链表的结点之中删除
 * 
 * @param mm 内存空间分布结构体
 * @param vma 待插入的VMA结构体
 */
void __vma_unlink_list(struct mm_struct * mm, struct vm_area_struct * vma);

/**
 * @brief 获取指定虚拟地址处映射的物理地址
 * 
 * @param mm 内存空间分布结构体
 * @param vaddr 虚拟地址
 * @return uint64_t 已映射的物理地址
 */
uint64_t __mm_get_paddr(struct mm_struct * mm, uint64_t vaddr);
