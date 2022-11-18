#pragma once
#include <libc/sys/types.h>

/**
 * @brief 系统内存信息结构体（单位：字节）
 *
 */
struct mstat_t
{
    uint64_t total;     // 计算机的总内存数量大小
    uint64_t used;      // 已使用的内存大小
    uint64_t free;      // 空闲物理页所占的内存大小
    uint64_t shared;    // 共享的内存大小
    uint64_t cache_used;     // 位于slab缓冲区中的已使用的内存大小
    uint64_t cache_free;     // 位于slab缓冲区中的空闲的内存大小
    uint64_t available; // 系统总空闲内存大小（包括kmalloc缓冲区）
};

int mkdir(const char *path, mode_t mode);

/**
 * @brief 获取系统的内存信息
 * 
 * @param stat 传入的内存信息结构体
 * @return int 错误码
 */
int mstat(struct mstat_t* stat);
int pipe(int *fd);