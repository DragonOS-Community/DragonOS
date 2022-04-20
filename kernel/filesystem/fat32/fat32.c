#include "fat32.h"
#include <common/kprint.h>
#include <driver/disk/ahci/ahci.h>
#include <filesystem/MBR.h>
#include <process/spinlock.h>
#include <mm/slab.h>

struct fat32_partition_info_t fat32_part_info[FAT32_MAX_PARTITION_NUM] = {0};
static int total_fat32_parts = 0;
static int max_fat32_parts_id = -1;
static uint64_t fat32_part_info_bmp[FAT32_MAX_PARTITION_NUM / 64 + 1] = {0};

static spinlock_t fat32_part_reg_lock;

/**
 * @brief 注册指定磁盘上的指定分区的fat32文件系统
 *
 * @param ahci_ctrl_num ahci控制器编号
 * @param ahci_port_num ahci控制器端口编号
 * @param part_num 磁盘分区编号
 *
 * @return int 全局fat32分区id
 */
int fat32_register_partition(uint8_t ahci_ctrl_num, uint8_t ahci_port_num, uint8_t part_num)
{
    for (int i = 0; i <= max_fat32_parts_id; ++i)
    {
        if (fat32_part_info_bmp[i / 64] & (1 << (i % 64)))
        {
            // 已经注册
            if (ahci_ctrl_num == fat32_part_info[i].ahci_ctrl_num && ahci_port_num == fat32_part_info[i].ahci_port_num && part_num == fat32_part_info[i].part_num)
                return i;
        }
    }

    // 注册分区
    spin_lock(&fat32_part_reg_lock);
    int current_part_id;
    for (int i = 0; i <= max_fat32_parts_id; ++i)
    {
        if ((fat32_part_info_bmp[i / 64] & (1 << (i % 64))) == 0)
        {
            current_part_id = i;
            break;
        }
    }
    ++max_fat32_parts_id;
    current_part_id = max_fat32_parts_id;
    fat32_part_info_bmp[current_part_id / 64] |= (1 << (current_part_id % 64));
    spin_unlock(&fat32_part_reg_lock);

    fat32_part_info[current_part_id].ahci_ctrl_num = ahci_ctrl_num;
    fat32_part_info[current_part_id].ahci_port_num = ahci_port_num;
    fat32_part_info[current_part_id].part_num = part_num;
    fat32_part_info[current_part_id].partition_id = current_part_id;

    struct MBR_disk_partition_table_t *DPT = MBR_read_partition_table(ahci_ctrl_num, ahci_port_num);

    //	for(i = 0 ;i < 512 ; i++)
    //		color_printk(PURPLE,WHITE,"%02x",buf[i]);
    printk_color(ORANGE, BLACK, "DPTE[0] start_LBA:%#018lx\ttype:%#018lx\n", DPT->DPTE[part_num].starting_LBA, DPT->DPTE[part_num].type);

    memset(buf, 0, 512);

    ahci_operation.transfer(ATA_CMD_READ_DMA_EXT, DPT->DPTE[part_num].starting_LBA, 1, (uint64_t)&buf, ahci_ctrl_num, ahci_port_num);

    fat32_part_info[current_part_id].bootsector = *(struct fat32_BootSector_t *)buf;

    // 计算数据区起始扇区号
    fat32_part_info[current_part_id].first_data_sector = DPT->DPTE[part_num].starting_LBA + fat32_part_info[current_part_id].bootsector.BPB_RsvdSecCnt +
                                                         fat32_part_info[current_part_id].bootsector.BPB_FATSz32 * fat32_part_info[current_part_id].bootsector.BPB_NumFATs;
    // 计算FAT1的起始扇区号
    fat32_part_info[current_part_id].FAT1_base_sector = DPT->DPTE[part_num].starting_LBA + fat32_part_info[current_part_id].bootsector.BPB_RsvdSecCnt;
    // 计算FAT2的起始扇区号
    fat32_part_info[current_part_id].FAT2_base_sector = fat32_part_info[current_part_id].FAT1_base_sector + fat32_part_info[current_part_id].bootsector.BPB_FATSz32;
    // 计算每个簇的大小
    fat32_part_info[current_part_id].bytes_per_clus = fat32_part_info[current_part_id].bootsector.BPB_BytesPerSec * fat32_part_info[current_part_id].bootsector.BPB_SecPerClus;

    kdebug("fat32_part_info[current_part_id].FAT1_base_sector=%#018lx", fat32_part_info[current_part_id].FAT1_base_sector);
    printk_color(ORANGE, BLACK, "FAT32 Boot Sector\n\tBPB_FSInfo:%#018lx\n\tBPB_BkBootSec:%#018lx\n\tBPB_TotSec32:%#018lx\n", fat32_part_info[current_part_id].bootsector.BPB_FSInfo, fat32_part_info[current_part_id].bootsector.BPB_BkBootSec, fat32_part_info[current_part_id].bootsector.BPB_TotSec32);

    memset(buf, 0, 512);
    ahci_operation.transfer(ATA_CMD_READ_DMA_EXT, DPT->DPTE[part_num].starting_LBA + fat32_part_info[current_part_id].bootsector.BPB_FSInfo, 1, (uint64_t)&buf, ahci_ctrl_num, ahci_port_num);

    fat32_part_info[current_part_id].fsinfo = *(struct fat32_FSInfo_t *)buf;
    //	for(i = 0 ;i < 512 ; i++)
    //		printk_color(PURPLE,WHITE,"%02x",buf[i]);
    printk_color(ORANGE, BLACK, "FAT32 FSInfo\n\tFSI_LeadSig:%#018lx\n\tFSI_StrucSig:%#018lx\n\tFSI_Free_Count:%#018lx\n", fat32_part_info[current_part_id].fsinfo.FSI_LeadSig, fat32_part_info[current_part_id].fsinfo.FSI_StrucSig, fat32_part_info[current_part_id].fsinfo.FSI_Free_Count);
    kdebug("fat32_part_info[part_id].bootsector.BPB_RootClus = %#018lx", fat32_part_info[current_part_id].bootsector.BPB_RootClus);
    return current_part_id;
}

/**
 * @brief 读取指定簇的FAT表项
 *
 * @param part_id 分区id
 * @param cluster
 * @return uint32_t 下一个簇的簇号
 */
uint32_t fat32_read_FAT_entry(uint32_t part_id, uint32_t cluster)
{

    uint32_t fat_ent_per_sec = (fat32_part_info[part_id].bootsector.BPB_BytesPerSec >> 2); // 该值应为2的n次幂
    uint32_t buf[256];
    memset(buf, 0, fat32_part_info[part_id].bootsector.BPB_BytesPerSec);

    ahci_operation.transfer(ATA_CMD_READ_DMA_EXT, fat32_part_info[part_id].FAT1_base_sector + (cluster / fat_ent_per_sec), 1, (uint64_t)&buf, fat32_part_info[part_id].ahci_ctrl_num, fat32_part_info[part_id].ahci_port_num);

    uint32_t ret = buf[cluster & (fat_ent_per_sec - 1)] & 0x0fffffff;

    return ret;
}

/**
 * @brief 写入指定簇的FAT表项
 *
 * @param part_id 分区id
 * @param cluster
 * @param value 要写入该fat表项的值
 * @return uint32_t 下一个簇的簇号
 */
uint32_t fat32_write_FAT_entry(uint32_t part_id, uint32_t cluster, uint32_t value)
{
    uint32_t fat_ent_per_sec = (fat32_part_info[part_id].bootsector.BPB_BytesPerSec >> 2); // 该值应为2的n次幂
    uint32_t buf[256];
    memset(buf, 0, fat32_part_info[part_id].bootsector.BPB_BytesPerSec);

    ahci_operation.transfer(ATA_CMD_READ_DMA_EXT, fat32_part_info[part_id].FAT1_base_sector + (cluster / fat_ent_per_sec), 1, (uint64_t)&buf, fat32_part_info[part_id].ahci_ctrl_num, fat32_part_info[part_id].ahci_port_num);

    buf[cluster & (fat_ent_per_sec - 1)] = (buf[cluster & (fat_ent_per_sec - 1)] & 0xf0000000) | (value & 0x0fffffff);
    // 向FAT1和FAT2写入数据
    ahci_operation.transfer(ATA_CMD_WRITE_DMA_EXT, fat32_part_info[part_id].FAT1_base_sector + (cluster / fat_ent_per_sec), 1, (uint64_t)&buf, fat32_part_info[part_id].ahci_ctrl_num, fat32_part_info[part_id].ahci_port_num);
    ahci_operation.transfer(ATA_CMD_WRITE_DMA_EXT, fat32_part_info[part_id].FAT2_base_sector + (cluster / fat_ent_per_sec), 1, (uint64_t)&buf, fat32_part_info[part_id].ahci_ctrl_num, fat32_part_info[part_id].ahci_port_num);

    return 0;
}

/**
 * @brief 在父目录中寻找指定的目录项
 *
 * @param part_id 分区id
 * @param name 目录项名字
 * @param name_len 目录项名字长度
 * @param dentry 父目录
 * @param flags
 * @return struct fat32_Directory_t* 目标目录项
 */
struct fat32_Directory_t *fat32_lookup(uint32_t part_id, char *name, int name_len, struct fat32_Directory_t *dentry, int flags)
{
    int errcode = 0;
    uint8_t *buf = kmalloc(fat32_part_info[part_id].bytes_per_clus, 0);
    memset(buf, 0, fat32_part_info[part_id].bytes_per_clus);

    // 计算父目录项的起始簇号
    uint32_t cluster = ((dentry->DIR_FstClusHI << 16) | (dentry->DIR_FstClusLO)) & 0x0fffffff;
    /*
    kdebug("dentry->DIR_FstClusHI=%#010lx", dentry->DIR_FstClusHI);
    kdebug("dentry->DIR_FstClusLo=%#010lx", dentry->DIR_FstClusLO);
    kdebug("cluster=%#010lx", cluster);
    */
    while (true)
    {

        // 计算父目录项的起始LBA扇区号
        uint64_t sector = fat32_part_info[part_id].first_data_sector + (cluster - 2) * fat32_part_info[part_id].bootsector.BPB_SecPerClus;
        //kdebug("fat32_part_info[part_id].bootsector.BPB_SecPerClus=%d",fat32_part_info[part_id].bootsector.BPB_SecPerClus);
        //kdebug("sector=%d",sector);

        // 读取父目录项的起始簇数据
        ahci_operation.transfer(ATA_CMD_READ_DMA_EXT, sector, fat32_part_info[part_id].bootsector.BPB_SecPerClus, (uint64_t)buf, 0, 0);
        //ahci_operation.transfer(ATA_CMD_READ_DMA_EXT, sector, fat32_part_info[part_id].bootsector.BPB_SecPerClus, (uint64_t)buf, fat32_part_info[part_id].ahci_ctrl_num, fat32_part_info[part_id].ahci_port_num);

        struct fat32_Directory_t *tmp_dEntry = (struct fat32_Directory_t *)buf;

        // 查找短目录项
        for (int i = 0; i < fat32_part_info[part_id].bytes_per_clus; i += 32, ++tmp_dEntry)
        {
            // 跳过长目录项
            if (tmp_dEntry->DIR_Attr == ATTR_LONG_NAME)
                continue;

            // 跳过无效页表项、空闲页表项
            if (tmp_dEntry->DIR_Name[0] == 0xe5 || tmp_dEntry->DIR_Name[0] == 0x00 || tmp_dEntry->DIR_Name[0] == 0x05)
                continue;

            // 找到长目录项，位于短目录项之前
            struct fat32_LongDirectory_t *tmp_ldEntry = (struct fat32_LongDirectory_t *)tmp_dEntry - 1;

            int js = 0;
            // 遍历每个长目录项
            while (tmp_ldEntry->LDIR_Attr == ATTR_LONG_NAME && tmp_ldEntry->LDIR_Ord != 0xe5)
            {
                // 比较name1
                for (int x = 0; x < 5; ++x)
                {
                    if (js > name_len && tmp_ldEntry->LDIR_Name1[x] == 0xffff)
                        continue;
                    else if (js > name_len || tmp_ldEntry->LDIR_Name1[x] != (uint16_t)(name[js++])) // 文件名不匹配，检索下一个短目录项
                        goto continue_cmp_fail;
                }

                // 比较name2
                for (int x = 0; x < 6; ++x)
                {
                    if (js > name_len && tmp_ldEntry->LDIR_Name2[x] == 0xffff)
                        continue;
                    else if (js > name_len || tmp_ldEntry->LDIR_Name2[x] != (uint16_t)(name[js++])) // 文件名不匹配，检索下一个短目录项
                        goto continue_cmp_fail;
                }

                // 比较name3
                for (int x = 0; x < 2; ++x)
                {
                    if (js > name_len && tmp_ldEntry->LDIR_Name3[x] == 0xffff)
                        continue;
                    else if (js > name_len || tmp_ldEntry->LDIR_Name3[x] != (uint16_t)(name[js++])) // 文件名不匹配，检索下一个短目录项
                        goto continue_cmp_fail;
                }

                if (js >= name_len) // 找到需要的目录项，返回
                {
                    struct fat32_Directory_t *p = (struct fat32_Directory_t *)kmalloc(sizeof(struct fat32_Directory_t), 0);
                    *p = *tmp_dEntry;
                    kfree(buf);
                    return p;
                }

                --tmp_ldEntry; // 检索下一个长目录项
            }

            // 不存在长目录项，匹配短目录项的基础名
            js = 0;
            for (int x = 0; x < 8; ++x)
            {
                switch (tmp_dEntry->DIR_Name[x])
                {
                case ' ':
                    if (!(tmp_dEntry->DIR_Attr & ATTR_DIRECTORY)) // 不是文件夹（是文件）
                    {
                        if (name[js] == '.')
                            continue;
                        else if (tmp_dEntry->DIR_Name[x] == name[js])
                        {
                            ++js;
                            break;
                        }
                        else
                            goto continue_cmp_fail;
                    }
                    else // 是文件夹
                    {
                        if (js < name_len && tmp_dEntry->DIR_Name[x] == name[js]) // 当前位正确匹配
                        {
                            ++js;
                            break; // 进行下一位的匹配
                        }
                        else if (js == name_len)
                            continue;
                        else
                            goto continue_cmp_fail;
                    }
                    break;

                // 当前位是字母
                case 'A' ... 'Z':
                case 'a' ... 'z':
                    if (tmp_dEntry->DIR_NTRes & LOWERCASE_BASE) // 为兼容windows系统，检测DIR_NTRes字段
                    {
                        if (js < name_len && (tmp_dEntry->DIR_Name[x] + 32 == name[js]))
                        {
                            ++js;
                            break;
                        }
                        else
                            goto continue_cmp_fail;
                    }
                    else
                    {
                        if (js < name_len && tmp_dEntry->DIR_Name[x] == name[js])
                        {
                            ++js;
                            break;
                        }
                        else
                            goto continue_cmp_fail;
                    }
                    break;
                case '0' ... '9':
                    if (js < name_len && tmp_dEntry->DIR_Name[x] == name[js])
                    {
                        ++js;
                        break;
                    }
                    else
                        goto continue_cmp_fail;

                    break;
                default:
                    ++js;
                    break;
                }
            }

            // 若短目录项为文件，则匹配扩展名
            if (!(tmp_dEntry->DIR_Attr & ATTR_DIRECTORY))
            {
                ++js;
                for (int x = 8; x < 11; ++x)
                {
                    switch (tmp_dEntry->DIR_Name[x])
                    {
                        // 当前位是字母
                    case 'A' ... 'Z':
                    case 'a' ... 'z':
                        if (tmp_dEntry->DIR_NTRes & LOWERCASE_EXT) // 为兼容windows系统，检测DIR_NTRes字段
                        {
                            if ((tmp_dEntry->DIR_Name[x] + 32 == name[js]))
                            {
                                ++js;
                                break;
                            }
                            else
                                goto continue_cmp_fail;
                        }
                        else
                        {
                            if (tmp_dEntry->DIR_Name[x] == name[js])
                            {
                                ++js;
                                break;
                            }
                            else
                                goto continue_cmp_fail;
                        }
                        break;
                    case '0' ... '9':
                    case ' ':
                        if (tmp_dEntry->DIR_Name[x] == name[js])
                        {
                            ++js;
                            break;
                        }
                        else
                            goto continue_cmp_fail;

                        break;

                    default:
                        goto continue_cmp_fail;
                        break;
                    }
                }
            }
            struct fat32_Directory_t *p = (struct fat32_Directory_t *)kmalloc(sizeof(struct fat32_Directory_t), 0);
            *p = *tmp_dEntry;
            kfree(buf);
            return p;
        continue_cmp_fail:;
        }

        // 当前簇没有发现目标文件名，寻找下一个簇
        cluster = fat32_read_FAT_entry(part_id, cluster);

        if (cluster >= 0x0ffffff7) // 寻找完父目录的所有簇，都没有找到目标文件名
        {
            kfree(buf);
            return NULL;
        }
    }
}

/**
 * @brief 按照路径查找文件
 *
 * @param part_id fat32分区id
 * @param path
 * @param flags
 * @return struct fat32_Directory_t*
 */
struct fat32_Directory_t *fat32_path_walk(uint32_t part_id, char *path, uint64_t flags)
{
    // 去除路径前的斜杠
    while (*path == '/')
        ++path;

    if ((!*path) || (*path == '\0'))
        return NULL;

    struct fat32_Directory_t *parent = (struct fat32_Directory_t *)kmalloc(sizeof(struct fat32_Directory_t), 0);
    char *dEntry_name = kmalloc(PAGE_4K_SIZE, 0);

    memset(parent, 0, sizeof(struct fat32_Directory_t));
    memset(dEntry_name, 0, PAGE_4K_SIZE);

    parent->DIR_FstClusLO = fat32_part_info[part_id].bootsector.BPB_RootClus & 0xffff;
    parent->DIR_FstClusHI = (fat32_part_info[part_id].bootsector.BPB_RootClus >> 16) & 0xffff;

    while (true)
    {
        // 提取出下一级待搜索的目录名或文件名，并保存在dEntry_name中
        char *tmp_path = path;
        while ((*path && *path != '\0') && (*path != '/'))
            ++path;
        int tmp_path_len = path - tmp_path;
        memcpy(dEntry_name, tmp_path, tmp_path_len);
        dEntry_name[tmp_path_len] = '\0';
        //kdebug("dEntry_name=%s", dEntry_name);
        struct fat32_Directory_t *next_dir = fat32_lookup(part_id, dEntry_name, tmp_path_len, parent, flags);

        if (next_dir == NULL)
        {
            // 搜索失败
            kerror("cannot find the file/dir : %s", dEntry_name);
            kfree(dEntry_name);
            kfree(parent);
            return NULL;
        }

        while (*path == '/')
            ++path;

        if ((!*path) || (*path == '\0')) //  已经到达末尾
        {
            if (flags & 1)  // 返回父目录
            {
                kfree(dEntry_name);
                kfree(next_dir);
                return parent;
            }

            kfree(dEntry_name);
            kfree(parent);
            return next_dir;
        }

        *parent = *next_dir;
        kfree(next_dir);
    }
}

void fat32_init()
{
    spin_init(&fat32_part_reg_lock);
}