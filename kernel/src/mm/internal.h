#pragma once

#include "mm.h"


// 当vma被成功合并后的返回值
#define __VMA_MERGED 1

/**
 * @brief 将vma结构体插入mm_struct的链表之中
 *
 * @param mm 内存空间分布结构体
 * @param vma 待插入的VMA结构体
 * @param prev 链表的前一个结点
 */
void __vma_link_list(struct mm_struct *mm, struct vm_area_struct *vma, struct vm_area_struct *prev);

/**
 * @brief 将vma给定结构体从vma链表的结点之中删除
 *
 * @param mm 内存空间分布结构体
 * @param vma 待插入的VMA结构体
 */
void __vma_unlink_list(struct mm_struct *mm, struct vm_area_struct *vma);

/**
 * @brief 获取指定虚拟地址处映射的物理地址
 *
 * @param mm 内存空间分布结构体
 * @param vaddr 虚拟地址
 * @return uint64_t 已映射的物理地址
 */
uint64_t __mm_get_paddr(struct mm_struct *mm, uint64_t vaddr);

/**
 * @brief 创建anon_vma，并将其与页面结构体进行绑定
 * 若提供的页面结构体指针为NULL，则只创建，不绑定
 *
 * @param page 页面结构体的指针
 * @param lock_page 是否将页面结构体加锁
 * @return struct anon_vma_t* 创建好的anon_vma
 */
struct anon_vma_t *__anon_vma_create_alloc(struct Page *page, bool lock_page);

/**
 * @brief 释放anon vma结构体
 *
 * @param anon_vma 待释放的anon_vma结构体
 * @return int 返回码
 */
int __anon_vma_free(struct anon_vma_t *anon_vma);

/**
 * @brief 将指定的vma加入到anon_vma的管理范围之中
 *
 * @param anon_vma 页面的anon_vma
 * @param vma 待加入的vma
 * @return int 返回码
 */
int __anon_vma_add(struct anon_vma_t *anon_vma, struct vm_area_struct *vma);

/**
 * @brief 从anon_vma的管理范围中删除指定的vma
 * (在进入这个函数之前，应该要对anon_vma加锁)
 * @param vma 将要取消对应的anon_vma管理的vma结构体
 * @return int 返回码
 */
int __anon_vma_del(struct vm_area_struct *vma);

/**
 * @brief 创建mmio对应的页结构体
 * 
 * @param paddr 物理地址
 * @return struct Page* 创建成功的page
 */
struct Page* __create_mmio_page_struct(uint64_t paddr);

// 判断给定的两个值是否跨越了2M边界
#define CROSS_2M_BOUND(val1, val2) ((val1 & PAGE_2M_MASK) != (val2 & PAGE_2M_MASK))