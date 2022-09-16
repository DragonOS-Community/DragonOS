#include <filesystem/devfs/devfs.h>
#include <filesystem/VFS/VFS.h>
#include <exception/softirq.h>
#include <common/kfifo.h>
#include <common/printk.h>
#include <common/wait_queue.h>
#include <lib/libKeyboard/keyboard.h>
#include <lib/libUI/textui.h>
#include "tty.h"

//stdin缓冲区
static struct kfifo_t tty_private_data;

static wait_queue_node_t tty_wait_queue;

extern struct kfifo_t kb_buf;

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
    if (kfifo_empty(&tty_private_data))
        wait_queue_sleep_on(&tty_wait_queue);

    count = (count > tty_private_data.size) ? tty_private_data.size : count;
    return kfifo_out(&tty_private_data, buf, count);
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



void getchar_from_keyboard(void *data)
{
    char buf[6]={0,0,0,0,0,0};
    int kb_buf_size=kb_buf.size;
    char character=0;
    if (kfifo_empty(&kb_buf))
        return;
    kfifo_out(&kb_buf, buf, kb_buf_size);
    character=keyboard_analyze_keycode(buf);
    textui_putchar(character, WHITE, BLACK);
    wait_queue_wakeup(&tty_wait_queue, PROC_UNINTERRUPTIBLE);
    if(character!=0){
        kfifo_in(&tty_private_data,&character,1);
    }
}
void tty_init(){
    //注册softirq
    register_softirq(TTY_GETCHAR_SIRQ, &getchar_from_keyboard, NULL);
    //初始化tty内存区域
    kfifo_alloc(&tty_private_data,MAX_STDIN_BUFFER_SIZE,0);
    wait_queue_init(&tty_wait_queue, NULL);
    //注册devfs
    devfs_register_device(DEV_TYPE_CHAR, CHAR_DEV_STYPE_TTY, &tty_fops);
    kinfo("tty driver registered.");
}