#include "keyboard.h"
#include "../interrupt/apic/apic.h"
#include "../../mm/mm.h"
#include "../../mm/slab.h"
#include "../../common/printk.h"

static struct keyboard_input_buffer *kb_buf_ptr = NULL;
static int shift_l, shift_r, ctrl_l, ctrl_r, alt_l, alt_r;
struct apic_IO_APIC_RTE_entry entry;

hardware_intr_controller keyboard_intr_controller =
    {
        .enable = apic_ioapic_enable,
        .disable = apic_ioapic_disable,
        .install = apic_ioapic_install,
        .uninstall = apic_ioapic_uninstall,
        .ack = apic_ioapic_edge_ack,

};

/**
 * @brief 键盘中断处理函数（中断上半部）
 *  将数据存入缓冲区
 * @param irq_num 中断向量号
 * @param param 参数
 * @param regs 寄存器信息
 */
void keyboard_handler(ul irq_num, ul param, struct pt_regs *regs)
{
    // 读取键盘输入的信息
    unsigned x = io_in8(0x60);
    printk_color(ORANGE, BLACK, "key_pressed:%02x\n", x);

    // 当头指针越过界时，恢复指向数组头部
    if (kb_buf_ptr->ptr_head == kb_buf_ptr + keyboard_buffer_size)
        kb_buf_ptr->ptr_head = kb_buf_ptr->buffer;

    if (kb_buf_ptr->count >= keyboard_buffer_size)
    {
        kwarn("Keyboard input buffer is full.");
        return;
    }

    *kb_buf_ptr->ptr_head = x;
    ++(kb_buf_ptr->count);
    ++(kb_buf_ptr->ptr_head);
}
/**
 * @brief 初始化键盘驱动程序的函数
 *
 */
void keyboard_init()
{
    // ======= 初始化键盘循环队列缓冲区 ===========

    // 申请键盘循环队列缓冲区的内存
    kb_buf_ptr = (struct keyboard_input_buffer *)kmalloc(sizeof(struct keyboard_input_buffer), 0);

    kb_buf_ptr->ptr_head = kb_buf_ptr;
    kb_buf_ptr->ptr_tail = kb_buf_ptr;
    kb_buf_ptr->count = 0;

    memset(kb_buf_ptr->buffer, 0, keyboard_buffer_size);

    // ======== 初始化中断RTE entry ==========

    entry.vector = 0x21;                // 设置中断向量号
    entry.deliver_mode = IO_APIC_FIXED; // 投递模式：混合
    entry.dest_mode = DEST_PHYSICAL;    // 物理模式投递中断
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
    wait_keyboard_write();
    io_out8(PORT_KEYBOARD_CONTROL, KEYBOARD_COMMAND_WRITE);
    wait_keyboard_write();
    io_out8(PORT_KEYBOARD_DATA, KEYBOARD_PARAM_INIT);
    wait_keyboard_write();

    // 执行一百万次nop，等待键盘控制器把命令执行完毕
    for (int i = 0; i < 1000; ++i)
        for (int j = 0; j < 1000; ++j)
            nop();
    shift_l = 0;
    shift_r = 0;
    ctrl_l = 0;
    ctrl_r = 0;
    alt_l = 0;
    alt_r = 0;

    // 注册中断处理程序
    irq_register(0x21, &entry, &keyboard_handler, (ul)kb_buf_ptr, &keyboard_intr_controller, "ps/2 keyboard");
}

/**
 * @brief 键盘驱动卸载函数
 *
 */
void keyboard_exit()
{
    irq_unregister(0x21);
    kfree((ul *)kb_buf_ptr);
}
