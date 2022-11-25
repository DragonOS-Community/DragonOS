#pragma once

#include <filesystem/VFS/VFS.h>
#include <common/lockref.h>
#include <common/spinlock.h>
#include <common/stdlib.h>
#include <common/stdio.h>
#include <common/string.h>
#include <process/process.h>
#include <common/list.h>

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
typedef struct procfs_sb_info_t procfs_sb_info_t;

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
 * @brief 私有信息结构
 *
 */
struct proc_data
{
    int readlen;
    char *rbuffer;
    int writelen;
    char *wbuffer;
};

/**
 * @brief 创建进程对应文件
 *
 * @param pid 进程号
 * @return int64_t 错误码
 */
int64_t create_proc_dir(long pid);
