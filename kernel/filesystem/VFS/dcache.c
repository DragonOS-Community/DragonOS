#include "internal.h"

/**
 * @brief 释放dentry
 * 
 * @param dentry 目标dentry
 */
void vfs_dentry_put(struct vfs_dir_entry_t * dentry)
{
    // todo: 加锁、放锁

    list_del(&dentry->child_node_list);// 从父dentry中删除

    // todo: 清除子目录的dentry

    dentry->dir_ops->release(dentry);

    kfree(dentry);
}