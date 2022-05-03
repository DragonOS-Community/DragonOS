#include "fat32.h"
#include <common/kprint.h>
#include <driver/disk/ahci/ahci.h>
#include <filesystem/MBR.h>
#include <process/spinlock.h>
#include <mm/slab.h>
#include <common/errno.h>

struct vfs_super_block_operations_t fat32_sb_ops;
struct vfs_dir_entry_operations_t fat32_dEntry_ops;
struct vfs_file_operations_t fat32_file_ops;
struct vfs_inode_operations_t fat32_inode_ops;

/**
 * @brief 注册指定磁盘上的指定分区的fat32文件系统
 *
 * @param ahci_ctrl_num ahci控制器编号
 * @param ahci_port_num ahci控制器端口编号
 * @param part_num 磁盘分区编号
 *
 * @return struct vfs_super_block_t * 文件系统的超级块
 */
struct vfs_superblock_t *fat32_register_partition(uint8_t ahci_ctrl_num, uint8_t ahci_port_num, uint8_t part_num)
{

    struct MBR_disk_partition_table_t *DPT = MBR_read_partition_table(ahci_ctrl_num, ahci_port_num);

    //	for(i = 0 ;i < 512 ; i++)
    //		color_printk(PURPLE,WHITE,"%02x",buf[i]);
    printk_color(ORANGE, BLACK, "DPTE[0] start_LBA:%#018lx\ttype:%#018lx\n", DPT->DPTE[part_num].starting_LBA, DPT->DPTE[part_num].type);
    uint8_t buf[512] = {0};

    // 读取文件系统的boot扇区
    ahci_operation.transfer(AHCI_CMD_READ_DMA_EXT, DPT->DPTE[part_num].starting_LBA, 1, (uint64_t)&buf, ahci_ctrl_num, ahci_port_num);

    // 挂载文件系统到vfs
    return vfs_mount_fs("FAT32", (void *)(&DPT->DPTE[part_num]), VFS_DPT_MBR, buf, ahci_ctrl_num, ahci_port_num, part_num);
}

/**
 * @brief 读取指定簇的FAT表项
 *
 * @param fsbi fat32超级块私有信息结构体
 * @param cluster 指定簇
 * @return uint32_t 下一个簇的簇号
 */
uint32_t fat32_read_FAT_entry(fat32_sb_info_t *fsbi, uint32_t cluster)
{
    // 计算每个扇区内含有的FAT表项数
    // FAT每项4bytes
    uint32_t fat_ent_per_sec = (fsbi->bytes_per_sec >> 2); // 该值应为2的n次幂

    uint32_t buf[256];
    memset(buf, 0, fsbi->bytes_per_sec);

    // 读取一个sector的数据，
    ahci_operation.transfer(AHCI_CMD_READ_DMA_EXT, fsbi->FAT1_base_sector + (cluster / fat_ent_per_sec), 1,
                            (uint64_t)&buf, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);

    // 返回下一个fat表项的值（也就是下一个cluster）
    return buf[cluster & (fat_ent_per_sec - 1)] & 0x0fffffff;
}

/**
 * @brief 写入指定簇的FAT表项
 *
 * @param fsbi fat32超级块私有信息结构体
 * @param cluster 指定簇
 * @param value 要写入该fat表项的值
 * @return uint32_t errcode
 */
uint32_t fat32_write_FAT_entry(fat32_sb_info_t *fsbi, uint32_t cluster, uint32_t value)
{
    // 计算每个扇区内含有的FAT表项数
    // FAT每项4bytes
    uint32_t fat_ent_per_sec = (fsbi->bytes_per_sec >> 2); // 该值应为2的n次幂
    uint32_t buf[256];
    memset(buf, 0, fsbi->bytes_per_sec);

    ahci_operation.transfer(AHCI_CMD_READ_DMA_EXT, fsbi->FAT1_base_sector + (cluster / fat_ent_per_sec), 1,
                            (uint64_t)&buf, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);

    buf[cluster & (fat_ent_per_sec - 1)] = (buf[cluster & (fat_ent_per_sec - 1)] & 0xf0000000) | (value & 0x0fffffff);
    // 向FAT1和FAT2写入数据
    ahci_operation.transfer(AHCI_CMD_WRITE_DMA_EXT, fsbi->FAT1_base_sector + (cluster / fat_ent_per_sec), 1,
                            (uint64_t)&buf, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);
    ahci_operation.transfer(AHCI_CMD_WRITE_DMA_EXT, fsbi->FAT2_base_sector + (cluster / fat_ent_per_sec), 1,
                            (uint64_t)&buf, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);

    return 0;
}

/**
 * @brief 在父目录中寻找指定的目录项
 *
 * @param parent_inode 父目录项的inode
 * @param dest_inode 搜索目标目录项的inode
 * @return struct vfs_dir_entry_t* 目标目录项
 */
struct vfs_dir_entry_t *fat32_lookup(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dest_dentry)
{
    int errcode = 0;

    struct fat32_inode_info_t *finode = (struct fat32_inode_info_t *)parent_inode->private_inode_info;
    fat32_sb_info_t *fsbi = (fat32_sb_info_t *)parent_inode->sb->private_sb_info;

    uint8_t *buf = kmalloc(fsbi->bytes_per_clus, 0);
    memset(buf, 0, fsbi->bytes_per_clus);

    // 计算父目录项的起始簇号
    uint32_t cluster = finode->first_clus;

    struct fat32_Directory_t *tmp_dEntry = NULL;

    while (true)
    {

        // 计算父目录项的起始LBA扇区号
        uint64_t sector = fsbi->first_data_sector + (cluster - 2) * fsbi->sec_per_clus;
        // kdebug("fat32_part_info[part_id].bootsector.BPB_SecPerClus=%d",fat32_part_info[part_id].bootsector.BPB_SecPerClus);
        // kdebug("sector=%d",sector);

        // 读取父目录项的起始簇数据
        ahci_operation.transfer(AHCI_CMD_READ_DMA_EXT, sector, fsbi->sec_per_clus, (uint64_t)buf, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);
        // ahci_operation.transfer(AHCI_CMD_READ_DMA_EXT, sector, fat32_part_info[part_id].bootsector.BPB_SecPerClus, (uint64_t)buf, fat32_part_info[part_id].ahci_ctrl_num, fat32_part_info[part_id].ahci_port_num);

        tmp_dEntry = (struct fat32_Directory_t *)buf;

        // 查找短目录项
        for (int i = 0; i < fsbi->bytes_per_clus; i += 32, ++tmp_dEntry)
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
                    if (js > dest_dentry->name_length && tmp_ldEntry->LDIR_Name1[x] == 0xffff)
                        continue;
                    else if (js > dest_dentry->name_length || tmp_ldEntry->LDIR_Name1[x] != (uint16_t)(dest_dentry->name[js++])) // 文件名不匹配，检索下一个短目录项
                        goto continue_cmp_fail;
                }

                // 比较name2
                for (int x = 0; x < 6; ++x)
                {
                    if (js > dest_dentry->name_length && tmp_ldEntry->LDIR_Name2[x] == 0xffff)
                        continue;
                    else if (js > dest_dentry->name_length || tmp_ldEntry->LDIR_Name2[x] != (uint16_t)(dest_dentry->name[js++])) // 文件名不匹配，检索下一个短目录项
                        goto continue_cmp_fail;
                }

                // 比较name3
                for (int x = 0; x < 2; ++x)
                {
                    if (js > dest_dentry->name_length && tmp_ldEntry->LDIR_Name3[x] == 0xffff)
                        continue;
                    else if (js > dest_dentry->name_length || tmp_ldEntry->LDIR_Name3[x] != (uint16_t)(dest_dentry->name[js++])) // 文件名不匹配，检索下一个短目录项
                        goto continue_cmp_fail;
                }

                if (js >= dest_dentry->name_length) // 找到需要的目录项，返回
                {
                    goto find_lookup_success;
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
                        if (dest_dentry->name[js] == '.')
                            continue;
                        else if (tmp_dEntry->DIR_Name[x] == dest_dentry->name[js])
                        {
                            ++js;
                            break;
                        }
                        else
                            goto continue_cmp_fail;
                    }
                    else // 是文件夹
                    {
                        if (js < dest_dentry->name_length && tmp_dEntry->DIR_Name[x] == dest_dentry->name[js]) // 当前位正确匹配
                        {
                            ++js;
                            break; // 进行下一位的匹配
                        }
                        else if (js == dest_dentry->name_length)
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
                        if (js < dest_dentry->name_length && (tmp_dEntry->DIR_Name[x] + 32 == dest_dentry->name[js]))
                        {
                            ++js;
                            break;
                        }
                        else
                            goto continue_cmp_fail;
                    }
                    else
                    {
                        if (js < dest_dentry->name_length && tmp_dEntry->DIR_Name[x] == dest_dentry->name[js])
                        {
                            ++js;
                            break;
                        }
                        else
                            goto continue_cmp_fail;
                    }
                    break;
                case '0' ... '9':
                    if (js < dest_dentry->name_length && tmp_dEntry->DIR_Name[x] == dest_dentry->name[js])
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
                            if ((tmp_dEntry->DIR_Name[x] + 32 == dest_dentry->name[js]))
                            {
                                ++js;
                                break;
                            }
                            else
                                goto continue_cmp_fail;
                        }
                        else
                        {
                            if (tmp_dEntry->DIR_Name[x] == dest_dentry->name[js])
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
                        if (tmp_dEntry->DIR_Name[x] == dest_dentry->name[js])
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
            goto find_lookup_success;
        continue_cmp_fail:;
        }

        // 当前簇没有发现目标文件名，寻找下一个簇
        cluster = fat32_read_FAT_entry(fsbi, cluster);

        if (cluster >= 0x0ffffff7) // 寻找完父目录的所有簇，都没有找到目标文件名
        {
            kfree(buf);
            return NULL;
        }
    }
find_lookup_success:; // 找到目标dentry
    struct vfs_index_node_t *p = (struct vfs_index_node_t *)kmalloc(sizeof(struct vfs_index_node_t), 0);
    memset(p, 0, sizeof(struct vfs_index_node_t));

    p->file_size = tmp_dEntry->DIR_FileSize;
    // 计算文件占用的扇区数, 由于最小存储单位是簇，因此需要按照簇的大小来对齐扇区
    p->blocks = (p->file_size + fsbi->bytes_per_clus - 1) / fsbi->bytes_per_sec;
    p->attribute = (tmp_dEntry->DIR_Attr & ATTR_DIRECTORY) ? VFS_ATTR_DIR : VFS_ATTR_FILE;
    p->sb = parent_inode->sb;
    p->file_ops = &fat32_file_ops;
    p->inode_ops = &fat32_inode_ops;

    // 为inode的与文件系统相关的信息结构体分配空间
    p->private_inode_info = (void *)kmalloc(sizeof(fat32_inode_info_t), 0);
    memset(p->private_inode_info, 0, sizeof(fat32_inode_info_t));
    finode = (fat32_inode_info_t *)p->private_inode_info;

    finode->first_clus = ((tmp_dEntry->DIR_FstClusHI << 16) | tmp_dEntry->DIR_FstClusLO) & 0x0fffffff;
    finode->dEntry_location_clus = cluster;
    finode->dEntry_location_clus_offset = tmp_dEntry - (struct fat32_Directory_t *)buf; //计算dentry的偏移量
    kdebug("finode->dEntry_location_clus=%#018lx", finode->dEntry_location_clus);
    kdebug("finode->dEntry_location_clus_offset=%#018lx", finode->dEntry_location_clus_offset);
    finode->create_date = tmp_dEntry->DIR_CrtDate;
    finode->create_time = tmp_dEntry->DIR_CrtTime;
    finode->write_date = tmp_dEntry->DIR_WrtDate;
    finode->write_time = tmp_dEntry->DIR_WrtTime;

    dest_dentry->dir_inode = p;
    kfree(buf);
    return dest_dentry;
}

/**
 * @brief 创建fat32文件系统的超级块
 *
 * @param DPTE 磁盘分区表entry
 * @param DPT_type 磁盘分区表类型
 * @param buf fat32文件系统的引导扇区
 * @return struct vfs_superblock_t* 创建好的超级块
 */
struct vfs_superblock_t *fat32_read_superblock(void *DPTE, uint8_t DPT_type, void *buf, int8_t ahci_ctrl_num, int8_t ahci_port_num, int8_t part_num)
{
    if (DPT_type != VFS_DPT_MBR) // 暂时只支持MBR分区表
    {
        kerror("fat32_read_superblock(): Unsupported DPT!");
        return NULL;
    }

    // 分配超级块的空间
    struct vfs_superblock_t *sb_ptr = (struct vfs_superblock_t *)kmalloc(sizeof(struct vfs_superblock_t), 0);
    memset(sb_ptr, 0, sizeof(struct vfs_superblock_t));

    sb_ptr->sb_ops = &fat32_sb_ops;
    sb_ptr->private_sb_info = kmalloc(sizeof(fat32_sb_info_t), 0);
    memset(sb_ptr->private_sb_info, 0, sizeof(fat32_sb_info_t));

    struct fat32_BootSector_t *fbs = (struct fat32_BootSector_t *)buf;

    fat32_sb_info_t *fsbi = (fat32_sb_info_t *)(sb_ptr->private_sb_info);

    // MBR分区表entry
    struct MBR_disk_partition_table_entry_t *MBR_DPTE = (struct MBR_disk_partition_table_entry_t *)DPTE;
    fsbi->ahci_ctrl_num = ahci_ctrl_num;
    fsbi->ahci_port_num = ahci_port_num;
    fsbi->part_num = part_num;

    fsbi->starting_sector = MBR_DPTE->starting_LBA;
    fsbi->sector_count = MBR_DPTE->total_sectors;
    fsbi->sec_per_clus = fbs->BPB_SecPerClus;
    fsbi->bytes_per_clus = fbs->BPB_SecPerClus * fbs->BPB_BytesPerSec;
    fsbi->bytes_per_sec = fbs->BPB_BytesPerSec;
    fsbi->first_data_sector = MBR_DPTE->starting_LBA + fbs->BPB_RsvdSecCnt + fbs->BPB_FATSz32 * fbs->BPB_NumFATs;
    fsbi->FAT1_base_sector = MBR_DPTE->starting_LBA + fbs->BPB_RsvdSecCnt;
    fsbi->FAT2_base_sector = fsbi->FAT1_base_sector + fbs->BPB_FATSz32;
    fsbi->sec_per_FAT = fbs->BPB_FATSz32;
    fsbi->NumFATs = fbs->BPB_NumFATs;
    fsbi->fsinfo_sector_addr_infat = fbs->BPB_FSInfo;
    fsbi->bootsector_bak_sector_addr_infat = fbs->BPB_BkBootSec;

    printk_color(ORANGE, BLACK, "FAT32 Boot Sector\n\tBPB_FSInfo:%#018lx\n\tBPB_BkBootSec:%#018lx\n\tBPB_TotSec32:%#018lx\n", fbs->BPB_FSInfo, fbs->BPB_BkBootSec, fbs->BPB_TotSec32);

    // fsinfo扇区的信息
    memset(&fsbi->fsinfo, 0, sizeof(struct fat32_FSInfo_t));
    ahci_operation.transfer(AHCI_CMD_READ_DMA_EXT, MBR_DPTE->starting_LBA + fbs->BPB_FSInfo, 1, (uint64_t)&fsbi->fsinfo, ahci_ctrl_num, ahci_port_num);
    printk_color(BLUE, BLACK, "FAT32 FSInfo\n\tFSI_LeadSig:%#018lx\n\tFSI_StrucSig:%#018lx\n\tFSI_Free_Count:%#018lx\n", fsbi->fsinfo.FSI_LeadSig, fsbi->fsinfo.FSI_StrucSig, fsbi->fsinfo.FSI_Free_Count);

    // 初始化超级块的dir entry
    sb_ptr->root = (struct vfs_dir_entry_t *)kmalloc(sizeof(struct vfs_dir_entry_t), 0);
    memset(sb_ptr->root, 0, sizeof(struct vfs_dir_entry_t));

    list_init(&sb_ptr->root->child_node_list);
    list_init(&sb_ptr->root->subdirs_list);

    sb_ptr->root->parent = sb_ptr->root;
    sb_ptr->root->dir_ops = &fat32_dEntry_ops;
    // 分配2个字节的name
    sb_ptr->root->name = (char *)(kmalloc(2, 0));
    sb_ptr->root->name[0] = '/';
    sb_ptr->root->name_length = 1;

    // 为root目录项分配index node
    sb_ptr->root->dir_inode = (struct vfs_index_node_t *)kmalloc(sizeof(struct vfs_index_node_t), 0);
    memset(sb_ptr->root->dir_inode, 0, sizeof(struct vfs_index_node_t));
    sb_ptr->root->dir_inode->inode_ops = &fat32_inode_ops;
    sb_ptr->root->dir_inode->file_ops = &fat32_file_ops;
    sb_ptr->root->dir_inode->file_size = 0;
    // 计算文件占用的扇区数, 由于最小存储单位是簇，因此需要按照簇的大小来对齐扇区
    sb_ptr->root->dir_inode->blocks = (sb_ptr->root->dir_inode->file_size + fsbi->bytes_per_clus - 1) / fsbi->bytes_per_sec;
    sb_ptr->root->dir_inode->attribute = VFS_ATTR_DIR;
    sb_ptr->root->dir_inode->sb = sb_ptr; // 反向绑定对应的超级块

    // 初始化inode信息
    sb_ptr->root->dir_inode->private_inode_info = kmalloc(sizeof(struct fat32_inode_info_t), 0);
    memset(sb_ptr->root->dir_inode->private_inode_info, 0, sizeof(struct fat32_inode_info_t));
    struct fat32_inode_info_t *finode = (struct fat32_inode_info_t *)sb_ptr->root->dir_inode->private_inode_info;

    finode->first_clus = fbs->BPB_RootClus;
    finode->dEntry_location_clus = 0;
    finode->dEntry_location_clus_offset = 0;
    finode->create_time = 0;
    finode->create_date = 0;
    finode->write_date = 0;
    finode->write_time;

    return sb_ptr;
}

/**
 * @brief todo: 写入superblock
 *
 * @param sb
 */
void fat32_write_superblock(struct vfs_superblock_t *sb)
{
}

/**
 * @brief 释放superblock的内存空间
 *
 * @param sb 要被释放的superblock
 */
void fat32_put_superblock(struct vfs_superblock_t *sb)
{
    kfree(sb->private_sb_info);
    kfree(sb->root->dir_inode->private_inode_info);
    kfree(sb->root->dir_inode);
    kfree(sb->root);
    kfree(sb);
}

/**
 * @brief 写入inode到硬盘上
 *
 * @param inode
 */
void fat32_write_inode(struct vfs_index_node_t *inode)
{
    fat32_inode_info_t *finode = inode->private_inode_info;

    if (finode->dEntry_location_clus == 0)
    {
        kerror("FAT32 error: Attempt to write the root inode");
        return;
    }

    fat32_sb_info_t *fsbi = (fat32_sb_info_t *)inode->sb->private_sb_info;

    // 计算目标inode对应数据区的LBA地址
    uint64_t fLBA = fsbi->first_data_sector + (finode->dEntry_location_clus - 2) * fsbi->sec_per_clus;
    kdebug("fLBA=%d", fLBA);
    kdebug("fsbi->first_data_sector=%d", fsbi->first_data_sector);

    struct fat32_Directory_t *buf = (struct fat32_Directory_t *)kmalloc(fsbi->bytes_per_clus, 0);
    memset(buf, 0, fsbi->bytes_per_clus);
    ahci_operation.transfer(AHCI_CMD_READ_DMA_EXT, fLBA, fsbi->sec_per_clus, (uint64_t)buf, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);

    // 计算目标dEntry所在的位置
    struct fat32_Directory_t *fdEntry = buf + finode->dEntry_location_clus_offset;

    // 写入fat32文件系统的dir_entry
    kdebug("inode->file_size=%#018lx", inode->file_size);
    kdebug("before   fdEntry->DIR_FileSize=%d", fdEntry->DIR_FileSize);
    fdEntry->DIR_FileSize = inode->file_size;
    fdEntry->DIR_FstClusLO = finode->first_clus & 0xffff;
    fdEntry->DIR_FstClusHI = (finode->first_clus >> 16) | (fdEntry->DIR_FstClusHI & 0xf000);

    kdebug("middle   fdEntry->DIR_FileSize=%d", fdEntry->DIR_FileSize);
    // 将dir entry写回磁盘
    ahci_operation.transfer(AHCI_CMD_WRITE_DMA_EXT, fLBA, fsbi->sec_per_clus, (uint64_t)buf, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);

    kfree(buf);
}

struct vfs_super_block_operations_t fat32_sb_ops =
    {
        .write_superblock = fat32_write_superblock,
        .put_superblock = fat32_put_superblock,
        .write_inode = fat32_write_inode,
};

// todo: compare
long fat32_compare(struct vfs_dir_entry_t *parent_dEntry, char *source_filename, char *dest_filename)
{
}
// todo: hash
long fat32_hash(struct vfs_dir_entry_t *dEntry, char *filename)
{
}
// todo: release
long fat32_release(struct vfs_dir_entry_t *dEntry)
{
}
// todo: iput
long fat32_iput(struct vfs_dir_entry_t *dEntry, struct vfs_index_node_t *inode)
{
}

/**
 * @brief fat32文件系统对于dEntry的操作
 *
 */
struct vfs_dir_entry_operations_t fat32_dEntry_ops =
    {
        .compare = fat32_compare,
        .hash = fat32_hash,
        .release = fat32_release,
        .iput = fat32_iput,
};

// todo: open
long fat32_open(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr)
{
    return VFS_SUCCESS;
}

// todo: close
long fat32_close(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr)
{
    return VFS_SUCCESS;
}

/**
 * @brief 从fat32文件系统读取数据
 *
 * @param file_ptr 文件描述符
 * @param buf 输出缓冲区
 * @param count 要读取的字节数
 * @param position 文件指针位置
 * @return long 执行成功：传输的字节数量    执行失败：错误码（小于0）
 */
long fat32_read(struct vfs_file_t *file_ptr, char *buf, int64_t count, long *position)
{

    struct fat32_inode_info_t *finode = (struct fat32_inode_info_t *)(file_ptr->dEntry->dir_inode->private_inode_info);
    fat32_sb_info_t *fsbi = (fat32_sb_info_t *)(file_ptr->dEntry->dir_inode->sb->private_sb_info);

    // First cluster num of the file
    uint64_t cluster = finode->first_clus;
    // kdebug("fsbi->bytes_per_clus=%d fsbi->sec_per_clus=%d finode->first_clus=%d cluster=%d", fsbi->bytes_per_clus, fsbi->sec_per_clus, finode->first_clus, cluster);

    // kdebug("fsbi->bytes_per_clus=%d", fsbi->bytes_per_clus);

    // clus offset in file
    uint64_t clus_offset_in_file = (*position) / fsbi->bytes_per_clus;
    // bytes offset in clus
    uint64_t bytes_offset = (*position) % fsbi->bytes_per_clus;

    if (!cluster)
        return -EFAULT;

    // find the actual cluster on disk of the specified position
    for (int i = 0; i < clus_offset_in_file; ++i)
        cluster = fat32_read_FAT_entry(fsbi, cluster);

    // 如果需要读取的数据边界大于文件大小
    if (*position + count > file_ptr->dEntry->dir_inode->file_size)
        count = file_ptr->dEntry->dir_inode->file_size - *position;

    // 剩余还需要传输的字节数量
    int64_t bytes_remain = count;

    // alloc buffer memory space for ahci transfer
    void *tmp_buffer = kmalloc(fsbi->bytes_per_clus, 0);

    int64_t retval = 0;
    do
    {

        memset(tmp_buffer, 0, fsbi->bytes_per_clus);
        uint64_t sector = fsbi->first_data_sector + (cluster - 2) * fsbi->sec_per_clus;

        // 读取一个簇的数据
        int errno = ahci_operation.transfer(AHCI_CMD_READ_DMA_EXT, sector, fsbi->sec_per_clus, (uint64_t)tmp_buffer, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);
        if (errno != AHCI_SUCCESS)
        {
            kerror("FAT32 FS(read) error!");
            retval = -EIO;
            break;
        }

        int64_t step_trans_len = 0; // 当前循环传输的字节数
        if (bytes_remain > (fsbi->bytes_per_clus - bytes_offset))
            step_trans_len = (fsbi->bytes_per_clus - bytes_offset);
        else
            step_trans_len = bytes_remain;

        if (((uint64_t)buf) < USER_MAX_LINEAR_ADDR)
            copy_to_user(buf, tmp_buffer + bytes_offset, step_trans_len);
        else
            memcpy(buf, tmp_buffer + bytes_offset, step_trans_len);

        bytes_remain -= step_trans_len;
        buf += step_trans_len;
        bytes_offset -= bytes_offset;

        *position += step_trans_len; // 更新文件指针

        cluster = fat32_read_FAT_entry(fsbi, cluster);
    } while (bytes_remain && (cluster < 0x0ffffff8) && cluster != 0);

    kfree(tmp_buffer);

    if (!bytes_remain)
        retval = count;

    return retval;
}

/**
 * @brief 在磁盘中寻找一个空闲的簇
 *
 * @param fsbi fat32超级块信息结构体
 * @return uint64_t 空闲簇号（找不到则返回0）
 */
uint64_t fat32_find_available_cluster(fat32_sb_info_t *fsbi)
{
    uint64_t sec_per_fat = fsbi->sec_per_FAT;

    // 申请1扇区的缓冲区
    uint32_t *buf = (uint32_t *)kmalloc(fsbi->bytes_per_sec, 0);
    int ent_per_sec = (fsbi->bytes_per_sec >> 2);
    for (int i = 0; i < sec_per_fat; ++i)
    {
        memset(buf, 0, fsbi->bytes_per_sec);

        ahci_operation.transfer(AHCI_CMD_READ_DMA_EXT, fsbi->FAT1_base_sector + i, 1, (uint64_t)buf, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);
        // 依次检查簇是否空闲
        for (int j = 0; j < ent_per_sec; ++j)
        {
            // 找到空闲簇
            if ((buf[j] & 0x0fffffff) == 0)
                return i * ent_per_sec + j;
        }
    }
    return 0;
}

/**
 * @brief 向fat32文件系统写入数据
 *
 * @param file_ptr 文件描述符
 * @param buf 输入写入的字节数
 * @param position 文件指针位置
 * @return long 执行成功：传输的字节数量    执行失败：错误码（小于0）
 */
long fat32_write(struct vfs_file_t *file_ptr, char *buf, int64_t count, long *position)
{
    struct fat32_inode_info_t *finode = (struct fat32_inode_info_t *)file_ptr->dEntry->dir_inode->private_inode_info;
    fat32_sb_info_t *fsbi = (fat32_sb_info_t *)(file_ptr->dEntry->dir_inode->sb->private_sb_info);

    // First cluster num of the file
    uint64_t cluster = finode->first_clus;
    int64_t flags = 0;

    // kdebug("fsbi->bytes_per_clus=%d fsbi->sec_per_clus=%d finode->first_clus=%d *position=%d", fsbi->bytes_per_clus, fsbi->sec_per_clus, finode->first_clus, *position);
    // kdebug("buf=%s", buf);
    // clus offset in file
    uint64_t clus_offset_in_file = (*position) / fsbi->bytes_per_clus;
    // bytes offset in clus
    uint64_t bytes_offset = (*position) % fsbi->bytes_per_clus;

    if (!cluster) // 起始簇号为0，说明是空文件
    {
        // 找一个可用的簇
        cluster = fat32_find_available_cluster(fsbi);
        flags = 1;
    }
    else
    {
        // 跳转到position所在的簇
        for (uint64_t i = 0; i < clus_offset_in_file; ++i)
            cluster = fat32_read_FAT_entry(fsbi, cluster);
    }
    // kdebug("cluster(start)=%d", cluster);
    //  没有可用的磁盘空间
    if (!cluster)
        return -ENOSPC;

    if (flags) // 空文件
    {
        // kdebug("empty file");
        finode->first_clus = cluster;
        // 写入目录项
        file_ptr->dEntry->dir_inode->sb->sb_ops->write_inode(file_ptr->dEntry->dir_inode);
        fat32_write_FAT_entry(fsbi, cluster, 0x0ffffff8); // 写入fat表项
    }

    int64_t bytes_remain = count;

    if (count < 0) // 要写入的字节数小于0
        return -EINVAL;

    uint64_t sector;
    int64_t retval = 0;

    void *tmp_buffer = kmalloc(fsbi->bytes_per_clus, 0);
    do
    {
        memset(tmp_buffer, 0, fsbi->bytes_per_clus);
        sector = fsbi->first_data_sector + (cluster - 2) * fsbi->sec_per_clus; // 计算对应的扇区
        if (!flags)                                                            // 当前簇已分配
        {
            // kdebug("read existed sec=%ld", sector);
            //  读取一个簇的数据
            int errno = ahci_operation.transfer(AHCI_CMD_READ_DMA_EXT, sector, fsbi->sec_per_clus, (uint64_t)tmp_buffer, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);
            if (errno != AHCI_SUCCESS)
            {
                // kerror("FAT32 FS(write)  read disk error!");
                retval = -EIO;
                break;
            }
        }

        int64_t step_trans_len = 0; // 当前循环传输的字节数
        if (bytes_remain > (fsbi->bytes_per_clus - bytes_offset))
            step_trans_len = (fsbi->bytes_per_clus - bytes_offset);
        else
            step_trans_len = bytes_remain;

        // kdebug("step_trans_len=%d, bytes_offset=%d", step_trans_len, bytes_offset);
        if (((uint64_t)buf) < USER_MAX_LINEAR_ADDR)
            copy_from_user(tmp_buffer + bytes_offset, buf, step_trans_len);
        else
            memcpy(tmp_buffer + bytes_offset, buf, step_trans_len);

        // 写入数据到对应的簇
        int errno = ahci_operation.transfer(AHCI_CMD_WRITE_DMA_EXT, sector, fsbi->sec_per_clus, (uint64_t)tmp_buffer, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);
        if (errno != AHCI_SUCCESS)
        {
            kerror("FAT32 FS(write)  write disk error!");
            retval = -EIO;
            break;
        }

        bytes_remain -= step_trans_len;
        buf += step_trans_len;
        bytes_offset -= bytes_offset;

        *position += step_trans_len; // 更新文件指针
        // kdebug("step_trans_len=%d", step_trans_len);

        int next_clus = 0;
        if (bytes_remain)
            next_clus = fat32_read_FAT_entry(fsbi, cluster);
        else
            break;
        if (next_clus >= 0x0ffffff8) // 已经到达了最后一个簇，需要分配新簇
        {
            next_clus = fat32_find_available_cluster(fsbi);
            if (!next_clus) // 没有空闲簇
            {
                kfree(tmp_buffer);
                return -ENOSPC;
            }
            // 将簇加入到文件末尾
            fat32_write_FAT_entry(fsbi, cluster, next_clus);
            fat32_write_FAT_entry(fsbi, next_clus, 0x0ffffff8);
            cluster = next_clus; // 切换当前簇
            flags = 1;           // 标记当前簇是新分配的簇
        }

    } while (bytes_remain);

    // 文件大小有增长
    if (*position > (file_ptr->dEntry->dir_inode->file_size))
    {
        file_ptr->dEntry->dir_inode->file_size = *position;
        file_ptr->dEntry->dir_inode->sb->sb_ops->write_inode(file_ptr->dEntry->dir_inode);
        kdebug("new file size=%ld", *position);
    }

    kfree(tmp_buffer);
    if (!bytes_remain)
        retval = count;
    // kdebug("retval=%lld", retval);
    return retval;
}
// todo: lseek
long fat32_lseek(struct vfs_file_t *file_ptr, long offset, long origin)
{
}
// todo: ioctl
long fat32_ioctl(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr, uint64_t cmd, uint64_t arg)
{
}

/**
 * @brief fat32文件系统，关于文件的操作
 *
 */
struct vfs_file_operations_t fat32_file_ops =
    {
        .open = fat32_open,
        .close = fat32_close,
        .read = fat32_read,
        .write = fat32_write,
        .lseek = fat32_lseek,
        .ioctl = fat32_ioctl,
};

// todo: create
long fat32_create(struct vfs_index_node_t *inode, struct vfs_dir_entry_t *dentry, int mode)
{
}

// todo: mkdir
int64_t fat32_mkdir(struct vfs_index_node_t *inode, struct vfs_dir_entry_t *dEntry, int mode)
{
}

// todo: rmdir
int64_t fat32_rmdir(struct vfs_index_node_t *inode, struct vfs_dir_entry_t *dEntry)
{
}

// todo: rename
int64_t fat32_rename(struct vfs_index_node_t *old_inode, struct vfs_dir_entry_t *old_dEntry, struct vfs_index_node_t *new_inode, struct vfs_dir_entry_t *new_dEntry)
{
}

// todo: getAttr
int64_t fat32_getAttr(struct vfs_dir_entry_t *dEntry, uint64_t *attr)
{
}

// todo: setAttr
int64_t fat32_setAttr(struct vfs_dir_entry_t *dEntry, uint64_t *attr)
{
}

struct vfs_inode_operations_t fat32_inode_ops =
    {
        .create = fat32_create,
        .mkdir = fat32_mkdir,
        .rmdir = fat32_rmdir,
        .lookup = fat32_lookup,
        .rename = fat32_rename,
        .getAttr = fat32_getAttr,
        .setAttr = fat32_setAttr,

};

struct vfs_filesystem_type_t fat32_fs_type =
    {
        .name = "FAT32",
        .fs_flags = 0,
        .read_superblock = fat32_read_superblock,
        .next = NULL,
};
void fat32_init()
{

    kinfo("Initializing FAT32...");

    // 在VFS中注册fat32文件系统
    vfs_register_filesystem(&fat32_fs_type);

    // 挂载根文件系统
    vfs_root_sb = fat32_register_partition(0, 0, 0);
    kinfo("FAT32 initialized.");
}