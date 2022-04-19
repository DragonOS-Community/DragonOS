#include "fat32.h"
#include <common/kprint.h>
#include <driver/disk/ahci/ahci.h>

/**
 * @brief 读取指定磁盘上的第0个分区的fat32文件系统
 * 
 * @param disk_num 
 */
void fat32_FS_init(int disk_num)
{
    int i;
    unsigned char buf[512];
    struct MBR_disk_partition_table_t DPT;
    struct fat32_BootSector_t fat32_bootsector;
    struct fat32_FSInfo_t fat32_fsinfo;

    memset(buf, 0, 512);
    ahci_operation.transfer(ATA_CMD_READ_DMA_EXT, 0, 1, (uint64_t)&buf, 0, 0);
    DPT = *(struct MBR_disk_partition_table_t *)buf;
    //	for(i = 0 ;i < 512 ; i++)
    //		color_printk(PURPLE,WHITE,"%02x",buf[i]);
    printk_color(ORANGE, BLACK, "DPTE[0] start_LBA:%#018lx\ttype:%#018lx\n", DPT.DPTE[0].starting_LBA, DPT.DPTE[0].type);

    memset(buf, 0, 512);

    ahci_operation.transfer(ATA_CMD_READ_DMA_EXT, DPT.DPTE[0].starting_LBA, 1, (uint64_t)&buf, 0, 0);

    fat32_bootsector = *(struct fat32_BootSector_t *)buf;
    //	for(i = 0 ;i < 512 ; i++)
    //		printk_color(PURPLE,WHITE,"%02x",buf[i]);
    printk_color(ORANGE, BLACK, "FAT32 Boot Sector\n\tBPB_FSInfo:%#018lx\n\tBPB_BkBootSec:%#018lx\n\tBPB_TotSec32:%#018lx\n", fat32_bootsector.BPB_FSInfo, fat32_bootsector.BPB_BkBootSec, fat32_bootsector.BPB_TotSec32);

    memset(buf, 0, 512);
        ahci_operation.transfer(ATA_CMD_READ_DMA_EXT, DPT.DPTE[0].starting_LBA+ fat32_bootsector.BPB_FSInfo, 1, (uint64_t)&buf, 0, 0);

    
    fat32_fsinfo = *(struct fat32_FSInfo_t *)buf;
    //	for(i = 0 ;i < 512 ; i++)
    //		printk_color(PURPLE,WHITE,"%02x",buf[i]);
    printk_color(ORANGE, BLACK, "FAT32 FSInfo\n\tFSI_LeadSig:%#018lx\n\tFSI_StrucSig:%#018lx\n\tFSI_Free_Count:%#018lx\n", fat32_fsinfo.FSI_LeadSig, fat32_fsinfo.FSI_StrucSig, fat32_fsinfo.FSI_Free_Count);
}