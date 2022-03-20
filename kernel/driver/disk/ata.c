#include "ata.h"
#include "../../common/kprint.h"
#include "../interrupt/apic/apic.h"

struct apic_IO_APIC_RTE_entry entry;

/**
 * @brief 硬盘中断上半部处理程序
 * 
 * @param irq_num 
 * @param param 
 * @param regs 
 */
void ata_disk_handler(ul irq_num, ul param, struct pt_regs *regs)
{

}

hardware_intr_controller ata_disk_intr_controller = 
{
    .enable = apic_ioapic_enable,
    .disable = apic_ioapic_disable,
    .install = apic_ioapic_install,
    .uninstall = apic_ioapic_uninstall,
    .ack = apic_ioapic_edge_ack,
};

/**
 * @brief 初始化ATA磁盘驱动程序
 *
 */
void ata_init()
{
    entry.vector = 0x2f;
    entry.deliver_mode = IO_APIC_FIXED;
    entry.dest_mode = DEST_PHYSICAL;
    entry.deliver_status = IDLE;
    entry.polarity = POLARITY_HIGH;
    entry.remote_IRR = IRR_RESET;
    entry.trigger_mode = EDGE_TRIGGER;
    entry.mask = MASKED;
    entry.reserved = 0;

    entry.destination.physical.reserved1 = 0;
    entry.destination.physical.reserved2 = 0;
    entry.destination.physical.phy_dest = 0; // 投递至BSP

    irq_register(entry.vector, &entry, &ata_disk_handler, 0, &ata_disk_intr_controller, "ATA Disk 1");

    io_out8(PORT_DISK1_STATUS_CTRL_REG, 0);   // 使能中断请求

    io_out8(PORT_DISK1_ERR_STATUS, 0);
    io_out8(PORT_DISK1_SECTOR_CNT, 0);
    io_out8(PORT_DISK1_LBA_7_0, 0);
    io_out8(PORT_DISK1_LBA_15_8, 0);
    io_out8(PORT_DISK1_LBA_23_16, 0);
    io_out8(PORT_DISK1_DEVICE_CONFIGURE_REG, 0);
    
    io_out8(PORT_DISK1_CONTROLLER_STATUS_CMD, 0xec);    // 获取硬件设备识别信息
}