/**
 * @file VFS.h
 * @author fslongjin (longjin@RinGoTek.cn)
 * @brief 虚拟文件系统
 * @version 0.1
 * @date 2022-04-20
 *
 * @copyright Copyright (c) 2022
 *
 */

#pragma once

struct vfs_file_operations_t
{
    long (*open)(void *not_used, void *not_used1);
    long (*close)(void *not_used, void *not_used1);
    long (*read)(void *not_used1, char *buf, int64_t count, long *position);
    long (*write)(void *not_used1, char *buf, int64_t count, long *position);
    long (*lseek)(void *not_used1, long offset, long origin);
    long (*ioctl)(void *not_used, void *not_used1, uint64_t cmd, uint64_t arg);
};

/**
 * @brief 初始化vfs
 *
 * @return int 错误码
 */
extern int vfs_init();
