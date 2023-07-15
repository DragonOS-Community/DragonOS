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

uint64_t sys_mm_stat(struct pt_regs *regs);

/**
 * @brief 获取系统当前的内存信息 （由于内存管理重构，该函数已经废弃）
 *
 * @return struct mm_stat_t 内存信息结构体
 */
struct mm_stat_t mm_stat()
{
    struct mm_stat_t tmp = {0};
    // 统计物理页的信息
    return tmp;
}

/**
 * @brief 获取内存信息的系统调用
 *
 * @param r8 返回的内存信息结构体的地址
 * @return uint64_t
 */
uint64_t sys_do_mstat(struct mm_stat_t *dst, bool from_user)
{
    if (dst == NULL)
        return -EFAULT;
    struct mm_stat_t stat = mm_stat();
    if (from_user)
        copy_to_user((void *)dst, &stat, sizeof(struct mm_stat_t));
    else
        memcpy((void *)dst, &stat, sizeof(struct mm_stat_t));
    return 0;
}