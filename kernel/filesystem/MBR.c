#include "MBR.h"
#include <common/kprint.h>
#include <driver/disk/ahci/ahci.h>

struct MBR_disk_partition_table_t MBR_partition_tables[MBR_MAX_AHCI_CTRL_NUM][MBR_MAX_AHCI_PORT_NUM] = {0};

/**
 * @brief 读取磁盘的分区表
 *
 * @param ahci_ctrl_num ahci控制器编号
 * @param ahci_port_num ahci端口编号
 */
struct MBR_disk_partition_table_t *MBR_read_partition_table(uint8_t ahci_ctrl_num, uint8_t ahci_port_num)
{
    unsigned char buf[512];
    memset(buf, 0, 512);
    ahci_operation.transfer(ATA_CMD_READ_DMA_EXT, 0, 1, (uint64_t)&buf, ahci_ctrl_num, ahci_port_num);
    MBR_partition_tables[ahci_ctrl_num][ahci_port_num] = *(struct MBR_disk_partition_table_t *)buf;
    return &MBR_partition_tables[ahci_ctrl_num][ahci_port_num];
}