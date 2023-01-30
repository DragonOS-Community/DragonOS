#pragma once
#include <common/glib.h>
#include "VFS.h"
/**
 * @brief 挂载点结构体(用来表示dentry被挂载其他文件系统之后，原先存在的数据)
 *
 */
struct mountpoint
{
    struct List mnt_list;                  // 挂载点串在一起的链表
    struct vfs_dir_entry_t *dentry;        // 被挂载前,当前目录项的dentry
    struct vfs_dir_entry_t *parent_dentry; // 被挂载前,父目录项的dentry
};

/**
 * @brief 初始化mount机制
 *
 * @return int 错误码
 */
int mount_init();

/**
 * @brief 将new_dentry挂载
 *
 * @param old_dentry 挂载点的dentry
 * @param new_dentry 待挂载的新的dentry(需使用vfs_alloc_dentry来分配)
 * @return int 错误码
 */
int do_mount(struct vfs_dir_entry_t *old_dentry, struct vfs_dir_entry_t *new_dentry);

/**
 * @brief 取消某个文件系统的挂载
 *
 * @param dentry 对应文件系统的根dentry
 * @return int 错误码
 */
int do_umount(struct vfs_dir_entry_t *dentry);

// 判断dentry是否是一个挂载点
#define D_MOUNTED(x) ((x)->d_flags & VFS_DF_MOUNTED)

/**
 * @brief 将给定的dentry标记为“不可挂载”
 *
 * @param dentry 目标dentry
 */
static inline void dont_mount(struct vfs_dir_entry_t *dentry)
{
    // todo: 对dentry加锁
    dentry->d_flags |= VFS_DF_CANNOT_MOUNT;
}

static inline void detach_mounts(struct vfs_dir_entry_t *dentry)
{
    if (!D_MOUNTED(dentry))
        return; // 如果当前文件夹不是一个挂载点，则直接返回

    // todo:如果当前文件夹是一个挂载点，则对同样挂载在当前文件夹下的dentry进行清理。以免造成内存泄露
    // 可参考 linux5.17或以上的detach_mounts()函数
}

/**
 * @brief 根据mountpoint的父目录dentry查找第一个符合条件的mountpoint结构体
 * 
 * @param dentry 父dentry
 * @return struct mountpoint* 第一个符合条件的mountpoint结构体的指针
 */
struct mountpoint *mount_find_mnt_list_by_parent(struct vfs_dir_entry_t *dentry);

/**
 * @brief 释放挂载点结构体
 * 
 * @param mp mountpoint结构体
 * @return int 错误码
 */
int mount_release_mountpoint(struct mountpoint* mp);