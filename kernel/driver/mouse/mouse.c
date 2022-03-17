#include "mouse.h"
#include "../interrupt/apic/apic.h"
#include "../../mm/mm.h"
#include "../../mm/slab.h"
#include "../../common/printk.h"
#include "../../common/kprint.h"

static struct mouse_input_buffer *mouse_buf_ptr = NULL;
static int c = 0;
struct apic_IO_APIC_RTE_entry mouse_entry;
static unsigned char mouse_id = 0;

/**
 * @brief 清空缓冲区
 *
 */
static void mouse_clear_buf()
{
    mouse_buf_ptr->ptr_head = mouse_buf_ptr->buffer;
    mouse_buf_ptr->ptr_tail = mouse_buf_ptr->buffer;
    mouse_buf_ptr->count = 0;
    memset(mouse_buf_ptr->buffer, 0, mouse_buffer_size);
}

/**
 * @brief 从缓冲队列中获取鼠标数据字节
 * @return 鼠标数据包的字节
 * 若缓冲队列为空则返回-1024
 */
static int mouse_get_scancode()
{
    // 缓冲队列为空
    if (mouse_buf_ptr->count == 0)
        while (!mouse_buf_ptr->count)
            nop();

    if (mouse_buf_ptr->ptr_tail == mouse_buf_ptr->buffer + mouse_buffer_size)
        mouse_buf_ptr->ptr_tail = mouse_buf_ptr->buffer;

    int ret = (int)((char)(*(mouse_buf_ptr->ptr_tail)));
    --(mouse_buf_ptr->count);
    ++(mouse_buf_ptr->ptr_tail);
    // printk("count=%d", mouse_buf_ptr->count);

    return ret;
}

/**
 * @brief 鼠标中断处理函数（中断上半部）
 *  将数据存入缓冲区
 * @param irq_num 中断向量号
 * @param param 参数
 * @param regs 寄存器信息
 */
void mouse_handler(ul irq_num, ul param, struct pt_regs *regs)
{
    // 读取鼠标输入的信息
    unsigned char x = io_in8(PORT_KEYBOARD_DATA);

    // 当头指针越过界时，恢复指向数组头部
    if (mouse_buf_ptr->ptr_head == mouse_buf_ptr->buffer + mouse_buffer_size)
        mouse_buf_ptr->ptr_head = mouse_buf_ptr->buffer;

    if (mouse_buf_ptr->count >= mouse_buffer_size)
    {
        // kwarn("mouse input buffer is full.");
        // return;
    }

    *mouse_buf_ptr->ptr_head = x;
    ++(mouse_buf_ptr->count);
    ++(mouse_buf_ptr->ptr_head);
    //printk("c=%d\tval = %d\n", ++c, x);
}

hardware_intr_controller mouse_intr_controller =
    {
        .enable = apic_ioapic_enable,
        .disable = apic_ioapic_disable,
        .install = apic_ioapic_install,
        .uninstall = apic_ioapic_uninstall,
        .ack = apic_ioapic_edge_ack,

};

/**
 * @brief 从键盘控制器读取mouse id
 *
 * @return unsigned char 鼠标id
 */
static unsigned char mouse_get_mouse_ID()
{
    // 读取鼠标的ID
    io_out8(PORT_KEYBOARD_CONTROL, KEYBOARD_COMMAND_SEND_TO_MOUSE);
    wait_keyboard_write();
    io_out8(PORT_KEYBOARD_DATA, MOUSE_GET_ID);
    wait_keyboard_write();
    mouse_id = io_in8(PORT_KEYBOARD_DATA);
    for (int i = 0; i < 1000; i++)
        for (int j = 0; j < 1000; j++)
            nop();
    return mouse_id;
}

/**
 * @brief 设置鼠标采样率
 *
 * @param hz 采样率
 */
int mouse_set_sample_rate(unsigned int hz)
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
        io_out8(PORT_KEYBOARD_CONTROL, KEYBOARD_COMMAND_SEND_TO_MOUSE);
        wait_keyboard_write();
        io_out8(PORT_KEYBOARD_DATA, MOUSE_SET_SAMPLING_RATE);
        wait_keyboard_write();
        for (int i = 0; i < 1000; i++)
            for (int j = 0; j < 1000; j++)
                nop();
        io_out8(PORT_KEYBOARD_CONTROL, KEYBOARD_COMMAND_SEND_TO_MOUSE);
        wait_keyboard_write();
        io_out8(PORT_KEYBOARD_DATA, hz);
        for (int i = 0; i < 1000; i++)
            for (int j = 0; j < 1000; j++)
                nop();
        wait_keyboard_write();

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
static int mouse_enable_scroll_wheel()
{
    if (mouse_id == 3)
        return SUCCESS;

    mouse_set_sample_rate(200);
    mouse_set_sample_rate(100);
    mouse_set_sample_rate(80);
    if (mouse_get_mouse_ID() != 3)
    {
        kerror("Cannot set mouse ID to 3");
        return EFAIL;
    }
    // 清空缓冲区，防止解析时产生错误
    mouse_clear_buf();
    return SUCCESS;
}
/**
 * @brief 使鼠标支持5键
 *  该模式下ID=4
 */
static int mouse_enable_5keys()
{
    if (mouse_id == 4)
        return SUCCESS;
    // 根据规范，应当先启用ID=3
    mouse_enable_scroll_wheel();

    mouse_set_sample_rate(200);
    mouse_set_sample_rate(200);
    mouse_set_sample_rate(80);
    if (mouse_get_mouse_ID() != 4)
    {
        kerror("Cannot set mouse ID to 4");
        return EFAIL;
    }
    // 清空缓冲区，防止解析时产生错误
    mouse_clear_buf();

    return SUCCESS;
}
/**
 * @brief 初始化鼠标驱动程序
 *
 */
void mouse_init()
{
    // 初始化鼠标读入队列缓冲区
    mouse_buf_ptr = (struct mouse_input_buffer *)kmalloc(sizeof(struct mouse_input_buffer), 0);
    mouse_buf_ptr->ptr_head = mouse_buf_ptr->buffer;
    mouse_buf_ptr->ptr_tail = mouse_buf_ptr->buffer;
    mouse_buf_ptr->count = 0;
    memset(mouse_buf_ptr->buffer, 0, mouse_buffer_size);

    // ======== 初始化中断RTE entry ==========

    mouse_entry.vector = MOUSE_INTR_VECTOR;   // 设置中断向量号
    mouse_entry.deliver_mode = IO_APIC_FIXED; // 投递模式：混合
    mouse_entry.dest_mode = DEST_PHYSICAL;    // 物理模式投递中断
    mouse_entry.deliver_status = IDLE;
    mouse_entry.trigger_mode = EDGE_TRIGGER; // 设置边沿触发
    mouse_entry.polarity = POLARITY_HIGH;    // 高电平触发
    mouse_entry.remote_IRR = IRR_RESET;
    mouse_entry.mask = MASKED;
    mouse_entry.reserved = 0;

    mouse_entry.destination.physical.reserved1 = 0;
    mouse_entry.destination.physical.reserved2 = 0;
    mouse_entry.destination.physical.phy_dest = 0; // 设置投递到BSP处理器

    // 注册中断处理程序
    irq_register(MOUSE_INTR_VECTOR, &mouse_entry, &mouse_handler, (ul)mouse_buf_ptr, &mouse_intr_controller, "ps/2 mouse");

    wait_keyboard_write();
    io_out8(PORT_KEYBOARD_CONTROL, KEYBOARD_COMMAND_ENABLE_MOUSE_PORT); // 开启鼠标端口
    for (int i = 0; i < 1000; i++)
        for (int j = 0; j < 1000; j++)
            nop();
    wait_keyboard_write();

    io_out8(PORT_KEYBOARD_CONTROL, KEYBOARD_COMMAND_SEND_TO_MOUSE);
    wait_keyboard_write();
    io_out8(PORT_KEYBOARD_DATA, MOUSE_ENABLE); // 允许鼠标设备发送数据包

    for (int i = 0; i < 1000; i++)
        for (int j = 0; j < 1000; j++)
            nop();
    wait_keyboard_write();
    io_out8(PORT_KEYBOARD_CONTROL, KEYBOARD_COMMAND_WRITE);
    wait_keyboard_write();
    io_out8(PORT_KEYBOARD_DATA, KEYBOARD_PARAM_INIT); // 设置键盘控制器
    for (int i = 0; i < 1000; i++)
        for (int j = 0; j < 1000; j++)
            nop();
    wait_keyboard_write();
    mouse_enable_5keys();
    mouse_get_mouse_ID();
    kdebug("mouse ID:%d", mouse_id);
    c = 0;
}

/**
 * @brief 卸载鼠标驱动程序
 *
 */
void mouse_exit()
{
    irq_unregister(MOUSE_INTR_VECTOR);
    kfree((ul *)mouse_buf_ptr);
}

/**
 * @brief 获取鼠标数据包
 *
 * @param packet 数据包的返回值
 * @return int 错误码
 */
int mouse_get_packet(void *packet)
{
    if (mouse_buf_ptr->count != 0)
        kdebug("at  get packet: count=%d", mouse_buf_ptr->count);
    int code = 0;
    switch (mouse_id)
    {
    case 0: // 3bytes 数据包
        if (mouse_buf_ptr->count < 3)
            return EFAIL;
        do
        {
            code = mouse_get_scancode();
            ((struct mouse_packet_3bytes *)packet)->byte0 = (unsigned char)code;
        } while (code == -1024);

        do
        {
            code = mouse_get_scancode();
            ((struct mouse_packet_3bytes *)packet)->movement_x = (char)code;
        } while (code == -1024);

        do
        {
            code = mouse_get_scancode();
            ((struct mouse_packet_3bytes *)packet)->movement_y = (char)code;
        } while (code == -1024);

        return SUCCESS;
        break;

    case 3: // 4bytes数据包
    case 4:
        if (mouse_buf_ptr->count < 4)
            return EFAIL;
        do
        {
            code = mouse_get_scancode();
            ((struct mouse_packet_4bytes *)packet)->byte0 = (unsigned char)code;
        } while (code == -1024);

        do
        {
            code = mouse_get_scancode();
            ((struct mouse_packet_4bytes *)packet)->movement_x = (char)code;
        } while (code == -1024);

        do
        {
            code = mouse_get_scancode();
            ((struct mouse_packet_4bytes *)packet)->movement_y = (char)code;
        } while (code == -1024);

        do
        {
            code = mouse_get_scancode();
            ((struct mouse_packet_4bytes *)packet)->byte3 = (char)code;
        } while (code == -1024);

        return SUCCESS;
        break;

    default: // Should not reach here
        kBUG("mouse_get_packet(): Invalid mouse_id!");
        return EFAIL;
        break;
    }
    return SUCCESS;
}