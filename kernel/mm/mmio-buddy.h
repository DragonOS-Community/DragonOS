#pragma once
#include <common/sys/types.h>
#include <common/glib.h>
#include "mm-types.h"
#include "mm.h"
#include "slab.h"

#define MMIO_BUDDY_MAX_EXP PAGE_1G_SHIFT
#define MMIO_BUDDY_MIN_EXP PAGE_4K_SHIFT
#define MMIO_BUDDY_REGION_COUNT (MMIO_BUDDY_MAX_EXP - MMIO_BUDDY_MIN_EXP + 1)

/**
 * @brief mmio伙伴系统内部的地址区域结构体
 *
 */
struct __mmio_buddy_addr_region
{
    struct List list;
    uint64_t vaddr; // 该内存对象起始位置的虚拟地址
};

/**
 * @brief 空闲页数组结构体
 *
 */
struct __mmio_free_region_list
{
    struct List list_head;
    int64_t num_free; // 空闲页的数量
};

/**
 * @brief buddy内存池
 *
 */
struct mmio_buddy_mem_pool
{
    uint64_t pool_start_addr; // 内存池的起始地址
    uint64_t pool_size;       // 内存池的内存空间总大小
    spinlock_t op_lock;       // 操作锁
    /**
     * @brief 空闲地址区域链表
     * 数组的第i个元素代表大小为2^(i+12)的内存区域
     */
    struct __mmio_free_region_list free_regions[MMIO_BUDDY_REGION_COUNT];
};

/**
 * @brief 释放address region结构体
 *
 * @param region 待释放的结构体
 */
static __always_inline void __mmio_buddy_release_addr_region(struct __mmio_buddy_addr_region *region)
{
    kfree(region);
}

/**
 * @brief 归还一块内存空间到buddy
 *
 * @param vaddr 虚拟地址
 * @param exp 内存空间的大小（2^exp）
 * @return int 返回码
 */
int __mmio_buddy_give_back(uint64_t vaddr, int exp);

/**
 * @brief 初始化mmio的伙伴系统
 *
 */
void mmio_buddy_init();

/**
 * @brief 从buddy中申请一块指定大小的内存区域
 *
 * @param exp 内存区域的大小(2^exp)
 * @return struct __mmio_buddy_addr_region* 符合要求的内存区域。没有满足要求的时候，返回NULL
 */
struct __mmio_buddy_addr_region *mmio_buddy_query_addr_region(int exp);