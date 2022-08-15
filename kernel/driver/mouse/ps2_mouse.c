#include "ps2_mouse.h"
#include <driver/interrupt/apic/apic.h>
#include <mm/mm.h>
#include <mm/slab.h>
#include <common/printk.h>
#include <common/kprint.h>

static struct ps2_mouse_input_buffer *ps2_mouse_buf_ptr = NULL;
static int c = 0;
struct apic_IO_APIC_RTE_entry ps2_mouse_entry;
static unsigned char ps2_mouse_id = 0;
struct ps2_mouse_packet_3bytes pak;
static int ps2_mouse_count = 0;
/**
 * @brief 清空缓冲区
 *
 */
static void ps2_mouse_clear_buf()
{
    ps2_mouse_buf_ptr->ptr_head = ps2_mouse_buf_ptr->buffer;
    ps2_mouse_buf_ptr->ptr_tail = ps2_mouse_buf_ptr->buffer;
    ps2_mouse_buf_ptr->count = 0;
    memset(ps2_mouse_buf_ptr->buffer, 0, ps2_mouse_buffer_size);
}

/**
 * @brief 从缓冲队列中获取鼠标数据字节
 * @return 鼠标数据包的字节
 * 若缓冲队列为空则返回-1024
 */
static int ps2_mouse_get_scancode()
{
    // 缓冲队列为空
    if (ps2_mouse_buf_ptr->count == 0)
        while (!ps2_mouse_buf_ptr->count)
            nop();

    if (ps2_mouse_buf_ptr->ptr_tail == ps2_mouse_buf_ptr->buffer + ps2_mouse_buffer_size)
        ps2_mouse_buf_ptr->ptr_tail = ps2_mouse_buf_ptr->buffer;

    int ret = (int)((char)(*(ps2_mouse_buf_ptr->ptr_tail)));
    --(ps2_mouse_buf_ptr->count);
    ++(ps2_mouse_buf_ptr->ptr_tail);
    // printk("count=%d", ps2_mouse_buf_ptr->count);

    return ret;
}

/**
 * @brief 鼠标中断处理函数（中断上半部）
 *  将数据存入缓冲区
 * @param irq_num 中断向量号
 * @param param 参数
 * @param regs 寄存器信息
 */
void ps2_mouse_handler(ul irq_num, ul param, struct pt_regs *regs)
{
    // 读取鼠标输入的信息
    unsigned char x = io_in8(PORT_KEYBOARD_DATA);

    // 当头指针越过界时，恢复指向数组头部
    if (ps2_mouse_buf_ptr->ptr_head == ps2_mouse_buf_ptr->buffer + ps2_mouse_buffer_size)
        ps2_mouse_buf_ptr->ptr_head = ps2_mouse_buf_ptr->buffer;

    if (ps2_mouse_buf_ptr->count >= ps2_mouse_buffer_size)
    {
        kwarn("ps2_mouse input buffer is full.");
        return;
    }

    *ps2_mouse_buf_ptr->ptr_head = x;
    ++(ps2_mouse_buf_ptr->count);
    ++(ps2_mouse_buf_ptr->ptr_head);
    printk("c=%d\tval = %d\n", ++c, x);
}

hardware_intr_controller ps2_mouse_intr_controller =
    {
        .enable = apic_ioapic_enable,
        .disable = apic_ioapic_disable,
        .install = apic_ioapic_install,
        .uninstall = apic_ioapic_uninstall,
        .ack = apic_ioapic_edge_ack,

};

/**
 * @brief 从键盘控制器读取ps2_mouse id
 *
 * @return unsigned char 鼠标id
 */
static unsigned char ps2_mouse_get_mouse_ID()
{
    // 读取鼠标的ID
    io_out8(PORT_KEYBOARD_CONTROL, KEYBOARD_COMMAND_SEND_TO_PS2_MOUSE);
    wait_keyboard_write();
    io_out8(PORT_KEYBOARD_DATA, PS2_MOUSE_GET_ID);
    wait_keyboard_write();
    ps2_mouse_id = io_in8(PORT_KEYBOARD_DATA);
    wait_keyboard_write();
    io_in8(PORT_KEYBOARD_DATA);
    for (int i = 0; i < 1000; i++)
        for (int j = 0; j < 1000; j++)
            nop();
    return ps2_mouse_id;
}

/**
 * @brief 设置鼠标采样率
 *
 * @param hz 采样率
 */
int ps2_mouse_set_sample_rate(unsigned int hz)
{
    switch (hz)
    {
    case 10:
    case 20:
    case 40:
    case 60:
    case 80:
    case 100:
    case 200:
        wait_keyboard_write();
        io_out8(PORT_KEYBOARD_CONTROL, KEYBOARD_COMMAND_SEND_TO_PS2_MOUSE);
        wait_keyboard_write();
        io_out8(PORT_KEYBOARD_DATA, PS2_MOUSE_SET_SAMPLING_RATE);
        wait_keyboard_write();
        io_in8(PORT_KEYBOARD_DATA);

        for (int i = 0; i < 1000; i++)
            for (int j = 0; j < 1000; j++)
                nop();

        io_out8(PORT_KEYBOARD_CONTROL, KEYBOARD_COMMAND_SEND_TO_PS2_MOUSE);
        wait_keyboard_write();
        io_out8(PORT_KEYBOARD_DATA, hz);
        for (int i = 0; i < 1000; i++)
            for (int j = 0; j < 1000; j++)
                nop();
        wait_keyboard_write();
        io_in8(PORT_KEYBOARD_DATA);

        break;

    default:
        return EINVALID_ARGUMENT;
        break;
    }
    return SUCCESS;
}
/**
 * @brief 使鼠标支持滚轮
 * 该模式下，鼠标ID=3
 */
static int ps2_mouse_enable_scroll_wheel()
{
    if (ps2_mouse_id == 3)
        return SUCCESS;

    ps2_mouse_set_sample_rate(200);
    ps2_mouse_set_sample_rate(100);
    ps2_mouse_set_sample_rate(80);
    if (ps2_mouse_get_mouse_ID() != 3)
    {
        kerror("Cannot set mouse ID to 3");
        return EFAIL;
    }
    // 清空缓冲区，防止解析时产生错误
    ps2_mouse_clear_buf();
    return SUCCESS;
}
/**
 * @brief 使鼠标支持5键
 *  该模式下ID=4
 */
static int ps2_mouse_enable_5keys()
{
    if (ps2_mouse_id == 4)
        return SUCCESS;
    // 根据规范，应当先启用ID=3
    ps2_mouse_enable_scroll_wheel();

    ps2_mouse_set_sample_rate(200);
    ps2_mouse_set_sample_rate(200);
    ps2_mouse_set_sample_rate(80);
    if (ps2_mouse_get_mouse_ID() != 4)
    {
        kerror("Cannot set ps2_mouse ID to 4");
        return EFAIL;
    }
    // 清空缓冲区，防止解析时产生错误
    ps2_mouse_clear_buf();

    return SUCCESS;
}
/**
 * @brief 初始化鼠标驱动程序
 *
 */
void ps2_mouse_init()
{
    // 初始化鼠标读入队列缓冲区
    ps2_mouse_buf_ptr = (struct ps2_mouse_input_buffer *)kzalloc(sizeof(struct ps2_mouse_input_buffer), 0);
    ps2_mouse_buf_ptr->ptr_head = ps2_mouse_buf_ptr->buffer;
    ps2_mouse_buf_ptr->ptr_tail = ps2_mouse_buf_ptr->buffer;
    ps2_mouse_buf_ptr->count = 0;
    memset(ps2_mouse_buf_ptr->buffer, 0, ps2_mouse_buffer_size);

    // ======== 初始化中断RTE entry ==========

    ps2_mouse_entry.vector = PS2_MOUSE_INTR_VECTOR;   // 设置中断向量号
    ps2_mouse_entry.deliver_mode = IO_APIC_FIXED; // 投递模式：混合
    ps2_mouse_entry.dest_mode = DEST_PHYSICAL;    // 物理模式投递中断
    ps2_mouse_entry.deliver_status = IDLE;
    ps2_mouse_entry.trigger_mode = EDGE_TRIGGER; // 设置边沿触发
    ps2_mouse_entry.polarity = POLARITY_HIGH;    // 高电平触发
    ps2_mouse_entry.remote_IRR = IRR_RESET;
    ps2_mouse_entry.mask = MASKED;
    ps2_mouse_entry.reserved = 0;

    ps2_mouse_entry.destination.physical.reserved1 = 0;
    ps2_mouse_entry.destination.physical.reserved2 = 0;
    ps2_mouse_entry.destination.physical.phy_dest = 0; // 设置投递到BSP处理器

    // 注册中断处理程序
    irq_register(PS2_MOUSE_INTR_VECTOR, &ps2_mouse_entry, &ps2_mouse_handler, (ul)ps2_mouse_buf_ptr, &ps2_mouse_intr_controller, "ps/2 mouse");

    wait_keyboard_write();
    io_out8(PORT_KEYBOARD_CONTROL, KEYBOARD_COMMAND_ENABLE_PS2_MOUSE_PORT); // 开启鼠标端口
    for (int i = 0; i < 1000; i++)
        for (int j = 0; j < 1000; j++)
            nop();
    wait_keyboard_write();
    io_in8(PORT_KEYBOARD_DATA);

    io_out8(PORT_KEYBOARD_CONTROL, KEYBOARD_COMMAND_SEND_TO_PS2_MOUSE);
    wait_keyboard_write();
    io_out8(PORT_KEYBOARD_DATA, PS2_MOUSE_ENABLE); // 允许鼠标设备发送数据包
    wait_keyboard_write();
    io_in8(PORT_KEYBOARD_DATA);

    for (int i = 0; i < 1000; i++)
        for (int j = 0; j < 1000; j++)
            nop();
    wait_keyboard_write();
    io_out8(PORT_KEYBOARD_CONTROL, KEYBOARD_COMMAND_WRITE);
    wait_keyboard_write();
    io_out8(PORT_KEYBOARD_DATA, KEYBOARD_PARAM_INIT); // 设置键盘控制器
    wait_keyboard_write();
    io_in8(PORT_KEYBOARD_DATA);
    for (int i = 0; i < 1000; i++)
        for (int j = 0; j < 1000; j++)
            nop();
    wait_keyboard_write();
    //ps2_mouse_enable_5keys();
    ps2_mouse_get_mouse_ID();
    ps2_mouse_set_sample_rate(30);
    ps2_mouse_clear_buf();
    kdebug("ps2_mouse ID:%d", ps2_mouse_id);
    c = 0;
    //ps2_mouse_count = 1;
}

/**
 * @brief 卸载鼠标驱动程序
 *
 */
void ps2_mouse_exit()
{
    irq_unregister(PS2_MOUSE_INTR_VECTOR);
    kfree((ul *)ps2_mouse_buf_ptr);
}

/**
 * @brief 获取鼠标数据包
 *
 * @param packet 数据包的返回值
 * @return int 错误码
 */
int ps2_mouse_get_packet(void *packet)
{
    // if (ps2_mouse_buf_ptr->count != 0)
    //     kdebug("at  get packet: count=%d", ps2_mouse_buf_ptr->count);
    int code = 0;
    switch (ps2_mouse_id)
    {
    case 0: // 3bytes 数据包
        if (ps2_mouse_buf_ptr->count < 4)
            return EFAIL;
        do
        {
            code = ps2_mouse_get_scancode();
            ((struct ps2_mouse_packet_3bytes *)packet)->byte0 = (unsigned char)code;
        } while (code == -1024);

        do
        {
            code = ps2_mouse_get_scancode();
            ((struct ps2_mouse_packet_3bytes *)packet)->movement_x = (char)code;
        } while (code == -1024);

        do
        {
            code = ps2_mouse_get_scancode();
            ((struct ps2_mouse_packet_3bytes *)packet)->movement_y = (char)code;
        } while (code == -1024);

        return SUCCESS;
        break;

    case 3: // 4bytes数据包
    case 4:
        if (ps2_mouse_buf_ptr->count < 5)
            return EFAIL;
        do
        {
            code = ps2_mouse_get_scancode();
            ((struct ps2_mouse_packet_4bytes *)packet)->byte0 = (unsigned char)code;
        } while (code == -1024);

        do
        {
            code = ps2_mouse_get_scancode();
            ((struct ps2_mouse_packet_4bytes *)packet)->movement_x = (char)code;
        } while (code == -1024);

        do
        {
            code = ps2_mouse_get_scancode();
            ((struct ps2_mouse_packet_4bytes *)packet)->movement_y = (char)code;
        } while (code == -1024);

        do
        {
            code = ps2_mouse_get_scancode();
            ((struct ps2_mouse_packet_4bytes *)packet)->byte3 = (char)code;
        } while (code == -1024);

        return SUCCESS;
        break;

    default: // Should not reach here
        kBUG("ps2_mouse_get_packet(): Invalid ps2_mouse_id!");
        return EFAIL;
        break;
    }
    return SUCCESS;
}

void analyze_mousecode()
{
    if(!ps2_mouse_buf_ptr->count)
        return;
    else printk_color(ORANGE, BLACK, "COUNT=%d\n", ps2_mouse_buf_ptr->count);
    unsigned char x = ps2_mouse_get_scancode();

    switch (ps2_mouse_count)
    {
    case 0:
        ps2_mouse_count++;
        break;

    case 1:
        pak.byte0 = x;
        ps2_mouse_count++;
        break;

    case 2:
        pak.movement_x = (char)x;
        ps2_mouse_count++;
        break;

    case 3:
        pak.movement_y = (char)x;
        ps2_mouse_count = 1;
        
        printk_color(RED, GREEN, "(M:%02x,X:%3d,Y:%3d)\tcount=%d\n", pak.byte0, pak.movement_x, pak.movement_y, ps2_mouse_buf_ptr->count);
        break;

    default:
        break;
    }
}