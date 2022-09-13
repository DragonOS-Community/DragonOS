#include "internal.h"
#include <common/kfifo.h>
#include <debug/bug.h>

/**
 * @brief 释放dentry
 *
 * @param dentry 目标dentry
 */
void vfs_dentry_put(struct vfs_dir_entry_t *dentry)
{
    int retval = 0;
    uint64_t in_value = 0;
    // todo: 加锁、放锁

    // 创建一个用来存放指向dentry的指针的fifo队列
    struct kfifo_t fifo;
    // 暂时假设队列大小为1024个元素
    // todo: 实现队列的自动扩容功能
    retval = kfifo_alloc(&fifo, 1024 * sizeof(uint64_t), 0);

    if (retval != 0)
        goto failed;

    // 将根dentry加入队列
    in_value = (uint64_t)dentry;
    kfifo_in(&fifo, &in_value, sizeof(uint64_t));
    list_del(&dentry->child_node_list); // 从父dentry中删除

    while (!kfifo_empty(&fifo))
    {
        // 取出队列中的下一个元素
        kfifo_out(&fifo, &dentry, sizeof(uint64_t));
        BUG_ON(dentry == NULL);
        struct List *list = &dentry->subdirs_list;
        if (!list_empty(list))
        {
            // 将当前dentry下的所有dentry加入队列
            do
            {
                list = list_next(list);
                in_value = (uint64_t)container_of(list, struct vfs_dir_entry_t, child_node_list);
                if (in_value != NULL)
                    kfifo_in(&fifo, &in_value, sizeof(uint64_t));

            } while (list_next(list) != (&dentry->subdirs_list));
        }

        // 释放inode
        vfs_free_inode(dentry->dir_inode);

        // 若当前dentry是否为挂载点，则umount
        if (is_local_mountpoint(dentry))
            do_umount(dentry);

        dentry->dir_ops->release(dentry);
        kfree(dentry);
    }
    kfifo_free_alloc(&fifo);
    return;
failed:;
    if (fifo.buffer != NULL)
        kfifo_free_alloc(&fifo);
    kerror("dentry_put failed.");
}

/**
 * @brief 释放inode
 *
 * @param inode 待释放的inode
 * @return int 错误码
 */
int vfs_free_inode(struct vfs_index_node_t *inode)
{
    --inode->ref_count;
    BUG_ON(inode->ref_count < 0);
    if (inode->ref_count == 0)
    {
        kfree(inode->private_inode_info);
        kfree(inode);
    }
    return 0;
}