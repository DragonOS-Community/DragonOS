/**
 * @file mm-stat.c
 * @author longjin(longjin@RinGoTek.cn)
 * @brief 查询内存信息
 * @version 0.1
 * @date 2022-08-06
 *
 * @copyright Copyright (c) 2022
 *
 */

#include "mm.h"
#include "slab.h"
#include <common/errno.h>
#include <process/ptrace.h>

extern const struct slab kmalloc_cache_group[16];

static int __empty_2m_pages(int zone);
static int __count_in_using_2m_pages(int zone);
static uint64_t __count_kmalloc_free();
static uint64_t __count_kmalloc_using();
static uint64_t __count_kmalloc_total();
uint64_t sys_mm_stat(struct pt_regs *regs);

/**
 * @brief 获取指定zone中的空闲2M页的数量
 *
 * @param zone 内存zone号
 * @return int 空闲2M页数量
 */
static int __count_empty_2m_pages(int zone)
{
    int zone_start = 0, zone_end = 0;

    uint64_t attr = 0;
    switch (zone)
    {
    case ZONE_DMA:
        // DMA区域
        zone_start = 0;
        zone_end = ZONE_DMA_INDEX;
        attr |= PAGE_PGT_MAPPED;
        break;
    case ZONE_NORMAL:
        zone_start = ZONE_DMA_INDEX;
        zone_end = ZONE_NORMAL_INDEX;
        attr |= PAGE_PGT_MAPPED;
        break;
    case ZONE_UNMAPPED_IN_PGT:
        zone_start = ZONE_NORMAL_INDEX;
        zone_end = ZONE_UNMAPPED_INDEX;
        attr = 0;
        break;
    default:
        kerror("In __count_empty_2m_pages: param: zone invalid.");
        // 返回错误码
        return -EINVAL;
        break;
    }

    uint64_t result = 0;
    for (int i = zone_start; i <= zone_end; ++i)
    {
        result += (memory_management_struct.zones_struct + i)->count_pages_free;
    }
    return result;
}

/**
 * @brief 获取指定zone中的正在使用的2M页的数量
 *
 * @param zone 内存zone号
 * @return int 空闲2M页数量
 */
static int __count_in_using_2m_pages(int zone)
{
    int zone_start = 0, zone_end = 0;

    uint64_t attr = 0;
    switch (zone)
    {
    case ZONE_DMA:
        // DMA区域
        zone_start = 0;
        zone_end = ZONE_DMA_INDEX;
        attr |= PAGE_PGT_MAPPED;
        break;
    case ZONE_NORMAL:
        zone_start = ZONE_DMA_INDEX;
        zone_end = ZONE_NORMAL_INDEX;
        attr |= PAGE_PGT_MAPPED;
        break;
    case ZONE_UNMAPPED_IN_PGT:
        zone_start = ZONE_NORMAL_INDEX;
        zone_end = ZONE_UNMAPPED_INDEX;
        attr = 0;
        break;
    default:
        kerror("In __count_in_using_2m_pages: param: zone invalid.");
        // 返回错误码
        return -EINVAL;
        break;
    }

    uint64_t result = 0;
    for (int i = zone_start; i <= zone_end; ++i)
    {
        result += (memory_management_struct.zones_struct + i)->count_pages_using;
    }
    return result;
}

/**
 * @brief 计算kmalloc缓冲区中的空闲内存
 *
 * @return uint64_t 空闲内存（字节）
 */
static uint64_t __count_kmalloc_free()
{
    uint64_t result = 0;
    for (int i = 0; i < sizeof(kmalloc_cache_group) / sizeof(struct slab); ++i)
    {
        result += kmalloc_cache_group[i].size * kmalloc_cache_group[i].count_total_free;
    }
    return result;
}

/**
 * @brief 计算kmalloc缓冲区中已使用的内存
 *
 * @return uint64_t 已使用的内存（字节）
 */
static uint64_t __count_kmalloc_using()
{
    uint64_t result = 0;
    for (int i = 0; i < sizeof(kmalloc_cache_group) / sizeof(struct slab); ++i)
    {
        result += kmalloc_cache_group[i].size * kmalloc_cache_group[i].count_total_using;
    }
    return result;
}

/**
 * @brief 计算kmalloc缓冲区中总共占用的内存
 *
 * @return uint64_t 缓冲区占用的内存（字节）
 */
static uint64_t __count_kmalloc_total()
{
    uint64_t result = 0;
    for (int i = 0; i < sizeof(kmalloc_cache_group) / sizeof(struct slab); ++i)
    {
        result += kmalloc_cache_group[i].size * (kmalloc_cache_group[i].count_total_free + kmalloc_cache_group[i].count_total_using);
    }
    return result;
}

/**
 * @brief 获取系统当前的内存信息(未上锁，不一定精准)
 * 
 * @return struct mm_stat_t 内存信息结构体
 */
struct mm_stat_t mm_stat()
{
    struct mm_stat_t tmp = {0};
    // 统计物理页的信息
    tmp.used = __count_in_using_2m_pages(ZONE_NORMAL) * PAGE_2M_SIZE;
    tmp.free = __count_empty_2m_pages(ZONE_NORMAL) * PAGE_2M_SIZE;
    tmp.total = tmp.used + tmp.free;
    tmp.shared = 0;
    // 统计kmalloc slab中的信息
    tmp.cache_free = __count_kmalloc_free();
    tmp.cache_used = __count_kmalloc_using();
    tmp.available = tmp.free + tmp.cache_free;
    return tmp;
}

/**
 * @brief 获取内存信息的系统调用
 *
 * @param r8 返回的内存信息结构体的地址
 * @return uint64_t
 */
uint64_t sys_mstat(struct pt_regs *regs)
{
    if (regs->r8 == NULL)
        return -EINVAL;
    struct mm_stat_t stat = mm_stat();
    if (regs->cs == (USER_CS | 0x3))
        copy_to_user((void *)regs->r8, &stat, sizeof(struct mm_stat_t));
    else
        memcpy((void *)regs->r8, &stat, sizeof(struct mm_stat_t));
    return 0;
}