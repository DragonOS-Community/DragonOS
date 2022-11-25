#pragma once

#include <common/list.h>
#include <common/lockref.h>
#include <common/spinlock.h>
#include <common/stdio.h>
#include <common/stdlib.h>
#include <common/string.h>
#include <filesystem/VFS/VFS.h>
#include <process/process.h>

/**
 * @brief 初始化procfs
 *
 */
void procfs_init();

/**
 * @brief proc文件系统的超级块信息结构体
 *
 */
struct procfs_sb_info_t
{
    struct lockref lockref; //该lockref包含自旋锁以及引用计数
};

/**
 * @brief procfs文件系统的结点私有信息
 *
 */
struct procfs_inode_info_t
{
    long pid;
    int type;
};

/**
 * @brief 创建进程对应文件
 *
 * @param pid 进程号
 * @return int64_t 错误码
 */
int64_t procfs_register_pid(long pid);
