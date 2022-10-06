#pragma once

#include "devfs.h"
#include <common/string.h>

extern struct vfs_super_block_operations_t devfs_sb_ops;
extern struct vfs_dir_entry_operations_t devfs_dentry_ops;
extern struct vfs_file_operations_t devfs_file_ops;
extern struct vfs_inode_operations_t devfs_inode_ops;
extern struct vfs_superblock_t devfs_sb;

/**
 * @brief 在devfs中注册字符设备（该函数只应被devfs调用）
 *
 * @param private_info inode私有信息
 * @param target_dentry 返回的dentry的指针
 * @return int 错误码
 */
int __devfs_chardev_register(struct devfs_private_inode_info_t *private_info, struct vfs_dir_entry_t **target_dentry);

/**
 * @brief 初始化chardev管理机制
 *
 */
void __devfs_chardev_init();

/**
 * @brief 在父dentry中寻找子dentry
 *
 * @param parent_dentry 父dentry结点
 * @param name 子目录项名称
 * @return struct vfs_dir_entry_t*
 */
static inline struct vfs_dir_entry_t *__devfs_find_dentry(struct vfs_dir_entry_t *parent_dentry, const char *name)
{
    struct List *list = &parent_dentry->subdirs_list;
    while (list_next(list) != &parent_dentry->subdirs_list)
    {
        list = list_next(list);
        // 获取目标dentry（由于是子目录项，因此是child_node_list）
        struct vfs_dir_entry_t *target_dent = container_of(list, struct vfs_dir_entry_t, child_node_list);
        if (strcmp(target_dent->name, name) == 0)
            return target_dent;
    }
    return NULL;
}

/**
 * @brief 在父目录下查找子目录
 *
 * @param parent_dentry 父目录
 * @param name 子目录名
 * @return struct vfs_dir_entry_t* 子目录的dentry （找不到则返回NULL）
 */
static inline struct vfs_dir_entry_t *__devfs_find_dir(struct vfs_dir_entry_t *parent_dentry, const char *name)
{
    struct vfs_dir_entry_t *target_dent = __devfs_find_dentry(parent_dentry, name);
    if (target_dent->dir_inode->attribute & VFS_IF_DIR) // 名称相符且为目录，则返回dentry
        return target_dent;
    else
        return NULL; // 否则直接返回空
}

/**
 * @brief 将dentry和inode进行绑定，并填充inode
 *
 * @param dentry 目标dentry
 * @param inode 目标inode
 * @param inode_attr inode的属性
 * @param private_inode_data inode私有信息
 */
static inline void __devfs_fill_inode(struct vfs_dir_entry_t *dentry, struct vfs_index_node_t *inode, uint64_t inode_attr, struct devfs_private_inode_info_t *private_inode_data)
{
    dentry->dir_inode = inode;
    dentry->dir_inode->file_ops = private_inode_data->f_ops;
    dentry->dir_inode->inode_ops = &devfs_inode_ops;

    dentry->dir_inode->private_inode_info = private_inode_data;
    dentry->dir_inode->sb = &devfs_sb;
    dentry->dir_inode->attribute = inode_attr;
    // 反向绑定inode
    private_inode_data->inode = dentry->dir_inode;
}

/**
 * @brief 填充dentry中的内容
 *
 * @param dentry 待填充的dentry
 * @param name dentry名称
 */
static inline void __devfs_fill_dentry(struct vfs_dir_entry_t *dentry, const char *name)
{
    strcpy(dentry->name, name);
    dentry->name_length = strlen(name);
    dentry->dir_ops = &devfs_dentry_ops;
}

/**
 * @brief 将dentry与父dentry进行绑定
 * @param parent 父目录项
 * @param dentry 子目录项
 */
#define __devfs_dentry_bind_parent(parent_dentry, dentry) ({                     \
    (dentry)->parent = (parent_dentry);                                          \
    list_append(&((parent_dentry)->subdirs_list), &((dentry)->child_node_list)); \
})
