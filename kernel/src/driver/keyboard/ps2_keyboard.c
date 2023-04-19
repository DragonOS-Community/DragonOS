#include "ps2_keyboard.h"
#include <driver/interrupt/apic/apic.h>
#include <mm/mm.h>
#include <mm/slab.h>
#include <common/printk.h>
#include <filesystem/vfs/VFS.h>
#include <common/wait_queue.h>
#include <common/spinlock.h>
#include <common/kfifo.h>

// 键盘输入缓冲区
static struct kfifo_t kb_buf;

// 缓冲区等待队列
static wait_queue_node_t ps2_keyboard_wait_queue;
extern void ps2_keyboard_register(struct vfs_file_operations_t *);
extern void ps2_keyboard_parse_keycode(uint8_t input); 

// 缓冲区读写锁
static spinlock_t ps2_kb_buf_rw_lock;

/**
 * @brief 重置ps2键盘输入缓冲区
 *
 * @param kbp 缓冲区对象指针
 */
static void ps2_keyboard_reset_buffer(struct kfifo_t *kbp)
{
    kfifo_reset(kbp);
}
struct apic_IO_APIC_RTE_entry entry;

hardware_intr_controller ps2_keyboard_intr_controller =
    {
        .enable = apic_ioapic_enable,
        .disable = apic_ioapic_disable,
        .install = apic_ioapic_install,
        .uninstall = apic_ioapic_uninstall,
        .ack = apic_ioapic_edge_ack,

};

/**
 * @brief 打开键盘文件
 *
 * @param inode 所在的inode
 * @param filp 文件指针
 * @return long
 */
long ps2_keyboard_open(struct vfs_index_node_t *inode, struct vfs_file_t *filp)
{
    ps2_keyboard_reset_buffer(&kb_buf);
    return 0;
}

/**
 * @brief 关闭键盘文件
 *
 * @param inode 所在的inode
 * @param filp 文件指针
 * @return long
 */
long ps2_keyboard_close(struct vfs_index_node_t *inode, struct vfs_file_t *filp)
{
    ps2_keyboard_reset_buffer(&kb_buf);
    return 0;
}

/**
 * @brief 键盘io控制接口
 *
 * @param inode 所在的inode
 * @param filp 键盘文件指针
 * @param cmd 命令
 * @param arg 参数
 * @return long
 */
long ps2_keyboard_ioctl(struct vfs_index_node_t *inode, struct vfs_file_t *filp, uint64_t cmd, uint64_t arg)
{
    switch (cmd)
    {
    case KEYBOARD_CMD_RESET_BUFFER:
        ps2_keyboard_reset_buffer(&kb_buf);
        break;

    default:
        break;
    }
    return 0;
}

/**
 * @brief 读取键盘文件的操作接口
 *
 * @param filp 文件指针
 * @param buf 输出缓冲区
 * @param count 要读取的字节数
 * @param position 读取的位置
 * @return long 读取的字节数
 */
long ps2_keyboard_read(struct vfs_file_t *filp, char *buf, int64_t count, long *position)
{
    // 缓冲区为空则等待
    if (kfifo_empty(&kb_buf))
        wait_queue_sleep_on(&ps2_keyboard_wait_queue);

    count = (count > kb_buf.size) ? kb_buf.size : count;
    return kfifo_out(&kb_buf, buf, count);
}

/**
 * @brief 键盘文件写入接口（无作用，空）
 *
 * @param filp
 * @param buf
 * @param count
 * @param position
 * @return long
 */
long ps2_keyboard_write(struct vfs_file_t *filp, char *buf, int64_t count, long *position)
{
    return 0;
}
/**
 * @brief ps2键盘驱动的虚拟文件接口
 *
 */
struct vfs_file_operations_t ps2_keyboard_fops =
    {
        .open = ps2_keyboard_open,
        .close = ps2_keyboard_close,
        .ioctl = ps2_keyboard_ioctl,
        .read = ps2_keyboard_read,
        .write = ps2_keyboard_write,
};

/**
 * @brief 键盘中断处理函数（中断上半部）
 *  将数据存入缓冲区
 * @param irq_num 中断向量号
 * @param param 参数
 * @param regs 寄存器信息
 */
void ps2_keyboard_handler(ul irq_num, ul buf_vaddr, struct pt_regs *regs)
{
    unsigned char x = io_in8(PORT_PS2_KEYBOARD_DATA);
    ps2_keyboard_parse_keycode((uint8_t)x);
    uint8_t count = kfifo_in((struct kfifo_t *)buf_vaddr, &x, sizeof(unsigned char));
    // if (count == 0)
    // {
    //     kwarn("ps2 keyboard buffer full.");
    //     return;
    // }

    wait_queue_wakeup(&ps2_keyboard_wait_queue, PROC_UNINTERRUPTIBLE);
    
}
/**
 * @brief 初始化键盘驱动程序的函数
 *
 */
void ps2_keyboard_init()
{

    // ======= 初始化键盘循环队列缓冲区 ===========

    // 初始化键盘循环队列缓冲区
    kfifo_alloc(&kb_buf, ps2_keyboard_buffer_size, 0);

    // ======== 初始化中断RTE entry ==========

    entry.vector = PS2_KEYBOARD_INTR_VECTOR; // 设置中断向量号
    entry.deliver_mode = IO_APIC_FIXED;      // 投递模式：混合
    entry.dest_mode = DEST_PHYSICAL;         // 物理模式投递中断
    entry.deliver_status = IDLE;
    entry.trigger_mode = EDGE_TRIGGER; // 设置边沿触发
    entry.polarity = POLARITY_HIGH;    // 高电平触发
    entry.remote_IRR = IRR_RESET;
    entry.mask = MASKED;
    entry.reserved = 0;

    entry.destination.physical.reserved1 = 0;
    entry.destination.physical.reserved2 = 0;
    entry.destination.physical.phy_dest = 0; // 设置投递到BSP处理器

    // ======== 初始化键盘控制器，写入配置值 =========
    wait_ps2_keyboard_write();
    io_out8(PORT_PS2_KEYBOARD_CONTROL, PS2_KEYBOARD_COMMAND_WRITE);
    wait_ps2_keyboard_write();
    io_out8(PORT_PS2_KEYBOARD_DATA, PS2_KEYBOARD_PARAM_INIT);
    wait_ps2_keyboard_write();

    // 执行一百万次nop，等待键盘控制器把命令执行完毕
    for (int i = 0; i < 1000; ++i)
        for (int j = 0; j < 1000; ++j)
            nop();

    wait_queue_init(&ps2_keyboard_wait_queue, NULL);
    // 初始化键盘缓冲区的读写锁
    spin_init(&ps2_kb_buf_rw_lock);

    // 注册中断处理程序
    irq_register(PS2_KEYBOARD_INTR_VECTOR, &entry, &ps2_keyboard_handler, (ul)&kb_buf, &ps2_keyboard_intr_controller, "ps/2 keyboard");

    // 先读一下键盘的数据，防止由于在键盘初始化之前，由于按键被按下从而导致接收不到中断。
    io_in8(PORT_PS2_KEYBOARD_DATA);
    // 将设备挂载到devfs
    ps2_keyboard_register(&ps2_keyboard_fops);

    kinfo("ps/2 keyboard registered.");
}

/**
 * @brief 键盘驱动卸载函数
 *
 */
void ps2_keyboard_exit()
{
    irq_unregister(PS2_KEYBOARD_INTR_VECTOR);
    kfifo_free_alloc(&kb_buf);

}
