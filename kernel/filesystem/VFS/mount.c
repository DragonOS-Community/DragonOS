#include "mount.h"
#include "VFS.h"
#include <common/glib.h>
#include <common/string.h>

static struct List mnt_list_head; // 挂载点链表头

/**
 * @brief 初始化mount机制
 *
 * @return int 错误码
 */
int mount_init()
{
    list_init(&mnt_list_head);
    return 0;
}

/**
 * @brief 将new_dentry挂载
 *
 * @param old_dentry 挂载点的dentry
 * @param new_dentry 待挂载的新的dentry(需使用vfs_alloc_dentry来分配)
 * @return int 错误码
 */
int do_mount(struct vfs_dir_entry_t *old_dentry, struct vfs_dir_entry_t *new_dentry)
{
    struct mountpoint *mp = (struct mountpoint *)kzalloc(sizeof(struct mountpoint), 0);
    list_init(&mp->mnt_list);
    mp->dentry = old_dentry;
    mp->parent_dentry = old_dentry->parent;
    
    kdebug("&new_dentry->name=%#018lx, &old_dentry->name=%#018lx", &new_dentry->name, &old_dentry->name);
    // 拷贝名称
    strncpy(new_dentry->name, old_dentry->name, old_dentry->name_length);

    list_init(&new_dentry->child_node_list);
    list_init(&new_dentry->subdirs_list);
    new_dentry->parent = old_dentry->parent;


    // 将新的dentry的list结点替换掉父dentry的列表中的old_dentry的list结点
    list_replace(&old_dentry->child_node_list, &new_dentry->child_node_list);
    list_append(&mnt_list_head, &mp->mnt_list);

    return 0;
}