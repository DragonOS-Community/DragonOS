#pragma once
#include "VFS.h"
#include "mount.h"

/**
 * @brief 判断是否可以删除指定的dentry
 *
 * 1、我们不能删除一个只读的dentry
 * 2、我们应当对这个dentry拥有写、执行权限（暂时还没有实现权限）
 * 3、如果dentry指向的是文件夹，而isdir为false，则不能删除
 * 3、如果dentry指向的是文件，而isdir为true，则不能删除
 * @param dentry 将要被删除的dentry
 * @param isdir 是否要删除文件夹
 * @return int 错误码
 */
int vfs_may_delete(struct vfs_dir_entry_t *dentry, bool isdir);

#define D_ISDIR(dentry) ((dentry)->dir_inode->attribute & VFS_IF_DIR)

// 判断是否为根目录
#define IS_ROOT(x) ((x) == (x)->parent)

/**
 * @brief 判断当前dentry是否为挂载点
 *
 * @param dentry
 */
static inline bool is_local_mountpoint(struct vfs_dir_entry_t *dentry)
{
    if (D_MOUNTED(dentry))
        return true;
    else
        return false;
}



/**
 * @brief 释放inode（要求已经对inode进行加锁后调用该函数）
 *
 * @param inode 待释放的inode
 * @return int 错误码
 *             当inode还有其他的使用者时，返回inode的使用者数量
 */
int vfs_free_inode(struct vfs_index_node_t * inode);