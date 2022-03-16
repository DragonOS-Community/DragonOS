#include "keyboard.h"
#include "../interrupt/apic/apic.h"
#include "../../mm/mm.h"
#include "../../mm/slab.h"
#include "../../common/printk.h"

static struct keyboard_input_buffer *kb_buf_ptr = NULL;

// 功能键标志变量
static bool shift_l, shift_r, ctrl_l, ctrl_r, alt_l, alt_r;
static bool gui_l, gui_r, apps, insert, home, pgup, del, end, pgdn, arrow_u, arrow_l, arrow_d, arrow_r;
static bool kp_forward_slash, kp_en;

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
    // printk_color(ORANGE, BLACK, "key_pressed:%02x\n", x);

    // 当头指针越过界时，恢复指向数组头部
    if (kb_buf_ptr->ptr_head == kb_buf_ptr->buffer + keyboard_buffer_size)
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

    kb_buf_ptr->ptr_head = kb_buf_ptr->buffer;
    kb_buf_ptr->ptr_tail = kb_buf_ptr->buffer;
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
    shift_l = false;
    shift_r = false;
    ctrl_l = false;
    ctrl_r = false;
    alt_l = false;
    alt_r = false;

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

/**
 * @brief 解析键盘扫描码
 *
 */
void keyboard_analyze_keycode()
{
    bool flag_make = false;

    int c = keyboard_get_scancode();
    // 循环队列为空
    if (c == -1)
        return;

    unsigned char scancode = (unsigned char)c;

    int key = 0;
    if (scancode == 0xE1) // Pause Break
    {
        key = PAUSE_BREAK;
        // 清除缓冲区中剩下的扫描码
        for (int i = 1; i < 6; ++i)
            if (keyboard_get_scancode() != pause_break_scan_code[i])
            {
                key = 0;
                break;
            }
    }
    else if (scancode == 0xE0) // 功能键, 有多个扫描码
    {
        // 获取下一个扫描码
        scancode = keyboard_get_scancode();
        switch (scancode)
        {
        case 0x2a: // print screen 按键被按下
            if (keyboard_get_scancode() == 0xe0)
                if (keyboard_get_scancode() == 0x37)
                {
                    key = PRINT_SCREEN;
                    flag_make = true;
                }
            break;
        case 0xb7: // print screen 按键被松开
            if (keyboard_get_scancode() == 0xe0)
                if (keyboard_get_scancode() == 0xaa)
                {
                    key = PRINT_SCREEN;
                    flag_make = false;
                }
            break;
        case 0x1d: // 按下右边的ctrl
            ctrl_r = true;
            key = OTHER_KEY;
            break;
        case 0x9d: // 松开右边的ctrl
            ctrl_r = false;
            key = OTHER_KEY;
            break;
        case 0x38: // 按下右边的alt
            alt_r = true;
            key = OTHER_KEY;
            break;
        case 0xb8: // 松开右边的alt
            alt_r = false;
            key = OTHER_KEY;
            break;
        case 0x5b:
            gui_l = true;
            key = OTHER_KEY;
            break;
        case 0xdb:
            gui_l = false;
            key = OTHER_KEY;
            break;
        case 0x5c:
            gui_r = true;
            key = OTHER_KEY;
            break;
        case 0xdc:
            gui_r = false;
            key = OTHER_KEY;
            break;
        case 0x5d:
            apps = true;
            key = OTHER_KEY;
            break;
        case 0xdd:
            apps = false;
            key = OTHER_KEY;
            break;
        case 0x52:
            insert = true;
            key = OTHER_KEY;
            break;
        case 0xd2:
            insert = false;
            key = OTHER_KEY;
            break;
        case 0x47:
            home = true;
            key = OTHER_KEY;
            break;
        case 0xc7:
            home = false;
            key = OTHER_KEY;
            break;
        case 0x49:
            pgup = true;
            key = OTHER_KEY;
            break;
        case 0xc9:
            pgup = false;
            key = OTHER_KEY;
            break;
        case 0x53:
            del = true;
            key = OTHER_KEY;
            break;
        case 0xd3:
            del = false;
            key = OTHER_KEY;
            break;
        case 0x4f:
            end = true;
            key = OTHER_KEY;
            break;
        case 0xcf:
            end = false;
            key = OTHER_KEY;
            break;
        case 0x51:
            pgdn = true;
            key = OTHER_KEY;
            break;
        case 0xd1:
            pgdn = false;
            key = OTHER_KEY;
            break;
        case 0x48:
            arrow_u = true;
            key = OTHER_KEY;
            break;
        case 0xc8:
            arrow_u = false;
            key = OTHER_KEY;
            break;
        case 0x4b:
            arrow_l = true;
            key = OTHER_KEY;
            break;
        case 0xcb:
            arrow_l = false;
            key = OTHER_KEY;
            break;
        case 0x50:
            arrow_d = true;
            key = OTHER_KEY;
            break;
        case 0xd0:
            arrow_d = false;
            key = OTHER_KEY;
            break;
        case 0x4d:
            arrow_r = true;
            key = OTHER_KEY;
            break;
        case 0xcd:
            arrow_r = false;
            key = OTHER_KEY;
            break;

        case 0x35: // 数字小键盘的 / 符号
            kp_forward_slash = true;
            key = OTHER_KEY;
            break;
        case 0xb5:
            kp_forward_slash = false;
            key = OTHER_KEY;
            break;
        case 0x1c:
            kp_en = true;
            key = OTHER_KEY;
            break;
        case 0x9c:
            kp_en = false;
            key = OTHER_KEY;
            break;

        default:
            key = OTHER_KEY;
            break;
        }
    }

    if (key == 0) // 属于第三类扫描码
    {
        // 判断按键是被按下还是抬起
        flag_make = ((scancode & FLAG_BREAK) ? 0 : 1);

        // 计算扫描码位于码表的第几行
        uint *key_row = &keycode_map_normal[(scancode & 0x7f) * MAP_COLS];
        unsigned char col = 0;
        // shift被按下
        if (shift_l || shift_r)
            col = 1;
        key = key_row[col];

        switch (scancode & 0x7f)
        {
        case 0x2a:
            shift_l = flag_make;
            key = 0;
            break;
        case 0x36:
            shift_r = flag_make;
            key = 0;
            break;
        case 0x1d:
            ctrl_l = flag_make;
            key = 0;
            break;
        case 0x38:
            ctrl_r = flag_make;
            key = 0;
            break;
        default:
            if (!flag_make)
                key = 0;
            break;
        }
        if (key)
            printk_color(ORANGE, BLACK, "%c", key);
    }
}

/**
 * @brief 从缓冲队列中获取键盘扫描码
 *
 */
int keyboard_get_scancode()
{
    // 缓冲队列为空
    if (kb_buf_ptr->count == 0)
        return -1;

    if (kb_buf_ptr->ptr_tail == kb_buf_ptr->buffer + keyboard_buffer_size)
        kb_buf_ptr->ptr_tail = kb_buf_ptr->buffer;

    int ret = (int)(*(kb_buf_ptr->ptr_tail));
    --(kb_buf_ptr->count);
    ++(kb_buf_ptr->ptr_tail);
    return ret;
}