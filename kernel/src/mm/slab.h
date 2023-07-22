#pragma once

#include "mm.h"

/**
 * @brief 通用内存分配函数
 *
 * @param size 要分配的内存大小
 * @param gfp 内存的flag
 * @return void* 分配得到的内存的指针
 */
extern void *kmalloc(unsigned long size, gfp_t gfp);

/**
 * @brief 从kmalloc申请一块内存，并将这块内存清空
 *
 * @param size 要分配的内存大小
 * @param gfp 内存的flag
 * @return void* 分配得到的内存的指针
 */
extern void *kzalloc(size_t size, gfp_t gfp);

/**
 * @brief 通用内存释放函数
 *
 * @param address 要释放的内存地址
 * @return unsigned long
 */
extern unsigned long kfree(void *address);
