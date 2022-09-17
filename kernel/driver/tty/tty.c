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
    kfifo_reset(&tty_private_data);
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
    kfifo_reset(&tty_private_data);
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
 * @brief tty文件写入接口
 *
 * @param filp
 * @param buf
 * @param count
 * @param position
 * @return long
 */
long tty_write(struct vfs_file_t *filp, char *buf, int64_t count, long *position)
{
    for(int64_t i=0;i<count;i++){
        textui_putchar(buf[i],WHITE,BLACK);
    }
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
    int character=0;
    if (kfifo_empty(&kb_buf))
        return;
    kfifo_out(&kb_buf, buf, kb_buf_size);
    character=keyboard_analyze_keycode(buf);
    if(character=='\b'){//处理退格
    #ifdef UNDERDEV
        if(!kfifo_empty(&tty_private_data)){
            if(tty_private_data.out_offset!=0){
                tty_private_data.out_offset--;
            }else{
                tty_private_data.out_offset=tty_private_data.total_size-1;
            }
            tty_private_data.size--;
            textui_putchar('\b', WHITE, BLACK);
        }
    #else
        textui_putchar(character, WHITE, BLACK);
        kfifo_in(&tty_private_data,&character,1);
    #endif
    }else{
        if(character<=0xff){
            textui_putchar(character, WHITE, BLACK);
            kfifo_in(&tty_private_data,&character,1);
        }else{
            textui_putchar('^',WHITE,BLACK);
            textui_putchar(character&0x7f, WHITE, BLACK);

        }
    }
    if(character=='\n')
        wait_queue_wakeup(&tty_wait_queue, PROC_UNINTERRUPTIBLE);
}
void tty_init(){
    //注册softirq，Todo: 改为驱动程序接口
    register_softirq(TTY_GETCHAR_SIRQ, &getchar_from_keyboard, NULL);
    //初始化tty内存区域
    kfifo_alloc(&tty_private_data,MAX_STDIN_BUFFER_SIZE,0);
    //kfifo_reset(&tty_private_data);
    wait_queue_init(&tty_wait_queue, NULL);
    //注册devfs
    devfs_register_device(DEV_TYPE_CHAR, CHAR_DEV_STYPE_TTY, &tty_fops);
    kinfo("tty driver registered.");
}