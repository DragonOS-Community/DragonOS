#include <filesystem/devfs/devfs.h>
#include <filesystem/VFS/VFS.h>
#include "tty.h"

static struct devfs_private_inode_info_t * tty_inode_private_data_ptr;  // 由devfs创建的inode私有信息指针
static int tty_private_data;

/**
 * @brief 打开tty文件
 *
 * @param inode 所在的inode
 * @param filp 文件指针
 * @return long
 */
long tty_open(struct vfs_index_node_t *inode, struct vfs_file_t *filp)
{
    filp->private_data = &tty_private_data;
    return 0;
}

/**
 * @brief 关闭tty文件
 *
 * @param inode 所在的inode
 * @param filp 文件指针
 * @return long
 */
long tty_close(struct vfs_index_node_t *inode, struct vfs_file_t *filp)
{
    filp->private_data = NULL;
    return 0;
}

/**
 * @brief tty控制接口
 *
 * @param inode 所在的inode
 * @param filp tty文件指针
 * @param cmd 命令
 * @param arg 参数
 * @return long
 */
long tty_ioctl(struct vfs_index_node_t *inode, struct vfs_file_t *filp, uint64_t cmd, uint64_t arg)
{
    switch (cmd)
    {
    default:
        break;
    }
    return 0;
}

/**
 * @brief 读取tty文件的操作接口
 *
 * @param filp 文件指针
 * @param buf 输出缓冲区
 * @param count 要读取的字节数
 * @param position 读取的位置
 * @return long 读取的字节数
 */
long tty_read(struct vfs_file_t *filp, char *buf, int64_t count, long *position)
{
    return 0;
}

/**
 * @brief tty文件写入接口（无作用，空）
 *
 * @param filp
 * @param buf
 * @param count
 * @param position
 * @return long
 */
long tty_write(struct vfs_file_t *filp, char *buf, int64_t count, long *position)
{
    return 0;
}

struct vfs_file_operations_t tty_fops={
    .open = tty_open,
    .close = tty_close,
    .ioctl = tty_ioctl,
    .read = tty_read,
    .write = tty_write,
};

void tty_init(){
    //注册devfs
    devfs_register_device(DEV_TYPE_CHAR, CHAR_DEV_STYPE_TTY, &tty_fops, &tty_inode_private_data_ptr);
    kinfo("tty driver registered. uuid=%d", tty_inode_private_data_ptr->uuid);
}