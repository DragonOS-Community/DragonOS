#pragma once

#include "mm.h"
#include <common/glib.h>
#include <common/printk.h>
#include <common/kprint.h>
#include <common/spinlock.h>

/**
 * @brief 通用内存分配函数
 *
 * @param size 要分配的内存大小
 * @param gfp 内存的flag
 * @return void* 分配得到的内存的指针
 */
// void *kmalloc(unsigned long size, gfp_t gfp);
extern void *kmalloc(unsigned long size, gfp_t gfp);

/**
 * @brief 从kmalloc申请一块内存，并将这块内存清空
 *
 * @param size 要分配的内存大小
 * @param gfp 内存的flag
 * @return void* 分配得到的内存的指针
 */
// static __always_inline void *kzalloc(size_t size, gfp_t gfp)
// {
//     return kmalloc(size, gfp | __GFP_ZERO);
// }
extern void *kzalloc(size_t size, gfp_t gfp);

/**
 * @brief 通用内存释放函数
 *
 * @param address 要释放的内存地址
 * @return unsigned long
 */
// unsigned long kfree(void *address);
extern unsigned long kfree(void *address);
