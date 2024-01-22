#include "ps2_mouse.h"
#include <arch/x86_64/driver/apic/apic.h>
#include <mm/mm.h>
#include <mm/slab.h>
#include <common/printk.h>
#include <common/kprint.h>

extern void ps2_mouse_driver_interrupt();

/**
 * @brief 鼠标中断处理函数（中断上半部）
 *  将数据存入缓冲区
 * @param irq_num 中断向量号
 * @param param 参数
 * @param regs 寄存器信息
 */
void ps2_mouse_handler(ul irq_num, ul param, struct pt_regs *regs)
{
    ps2_mouse_driver_interrupt();
}

struct apic_IO_APIC_RTE_entry ps2_mouse_entry;

hardware_intr_controller ps2_mouse_intr_controller =
    {
        .enable = apic_ioapic_enable,
        .disable = apic_ioapic_disable,
        .install = apic_ioapic_install,
        .uninstall = apic_ioapic_uninstall,
        .ack = apic_ioapic_edge_ack,

};

/**
 * @brief 初始化鼠标驱动程序
 *
 */
void ps2_mouse_init()
{
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
    irq_register(PS2_MOUSE_INTR_VECTOR, &ps2_mouse_entry, &ps2_mouse_handler, 0, &ps2_mouse_intr_controller, "ps/2 mouse");
}