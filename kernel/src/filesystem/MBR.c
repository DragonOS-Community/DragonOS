#include "MBR.h"
#include <common/kprint.h>
#include <driver/disk/ahci/ahci.h>

struct MBR_disk_partition_table_t MBR_partition_tables[MBR_MAX_AHCI_CTRL_NUM][MBR_MAX_AHCI_PORT_NUM] = {0};

/**
 * @brief 读取磁盘的分区表
 *
 * @param ahci_ctrl_num ahci控制器编号
 * @param ahci_port_num ahci端口编号
 * @param buf 输出缓冲区（512字节）
 */
int MBR_read_partition_table(struct blk_gendisk *gd, void *buf)
{
    return gd->fops->transfer(gd, AHCI_CMD_READ_DMA_EXT, 0, 1, (uint64_t)buf);
}