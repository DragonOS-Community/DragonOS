#pragma once
#include <common/glib.h>

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
int do_umount(struct vfs_dir_entry_t* dentry);