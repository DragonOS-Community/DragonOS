#include "internal.h"
#include <common/kfifo.h>
#include <debug/bug.h>

/**
 * @brief 释放dentry，并视情况自动释放inode. 在调用该函数前，需要将dentry加锁。
 *
 * @param dentry 目标dentry
 *
 * @return 错误码
 *          注意，当dentry指向文件时，如果返回值为正数，则表示在释放了该dentry后，该dentry指向的inode的引用计数。
 */
int vfs_dentry_put(struct vfs_dir_entry_t *dentry)
{
    int retval = 0;
    uint64_t in_value = 0;
    struct kfifo_t fifo = {0};
    const struct vfs_dir_entry_t *start_dentry = dentry;

    // 引用计数大于1时，尝试释放dentry的话，抛出错误信息
    if (unlikely(dentry->lockref.count > 1))
    {
        BUG_ON(1);
        retval = -EBUSY;
        goto out;
    }

    if (D_ISDIR(dentry))
    {

        // 创建一个用来存放指向dentry的指针的fifo队列
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
            if (unlikely(dentry != start_dentry))
                spin_lock(&dentry->lockref.lock);
            if (dentry->lockref.count > 1)
            {
                if (unlikely(dentry != start_dentry))
                    spin_unlock(&dentry->lockref.lock);
                continue;
            }
            // 释放inode
            spin_lock(&dentry->dir_inode->lockref.lock);
            retval = vfs_free_inode(dentry->dir_inode);
            if (retval > 0) // 还有其他的dentry引用着这个inode
            {
                spin_unlock(&dentry->dir_inode->lockref.lock);
                retval = 0;
            }

            // 若当前dentry是否为挂载点，则umount
            if (is_local_mountpoint(dentry))
                do_umount(dentry);
            if (dentry->dir_ops->release != NULL)
                dentry->dir_ops->release(dentry);
            kfree(dentry);
        }
        kfifo_free_alloc(&fifo);
        retval = 0;
        goto out;
    }
    else // 是文件或设备
    {
        kdebug("to put dentry: file: %s", dentry->name);
        list_del(&dentry->child_node_list); // 从父dentry中删除
        // 释放inode
        spin_lock(&dentry->dir_inode->lockref.lock);
        retval = vfs_free_inode(dentry->dir_inode);
        kdebug("retval=%d", retval);
        if (retval > 0) // 还有其他的dentry引用着这个inode
            spin_unlock(&dentry->dir_inode->lockref.lock);

        if (dentry->dir_ops->release != NULL)
            dentry->dir_ops->release(dentry);
        kfree(dentry);
        goto out;
    }
failed:;
    if (fifo.buffer != NULL)
        kfifo_free_alloc(&fifo);
    kerror("dentry_put failed.");
out:;
    // 在这里不用释放dentry的锁，因为dentry已经被释放掉了
    return retval;
}

/**
 * @brief 释放inode（要求已经对inode进行加锁后调用该函数）
 *
 * @param inode 待释放的inode
 * @return int 错误码
 *             当inode还有其他的使用者时，返回inode的使用者数量
 */
int vfs_free_inode(struct vfs_index_node_t *inode)
{
    --inode->lockref.count;
    BUG_ON(inode->lockref.count < 0);
    if (inode->lockref.count == 0)
    {
        kfree(inode->private_inode_info);
        kfree(inode);
        return 0;
    }
    else // 如果inode没有被释放
        return inode->lockref.count;
}