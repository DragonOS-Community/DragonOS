#include "fat32.h"
#include "fat_ent.h"
#include "internal.h"
#include <common/errno.h>
#include <common/kprint.h>
#include <common/spinlock.h>
#include <common/stdio.h>
#include <common/string.h>
#include <driver/disk/ahci/ahci.h>
#include <filesystem/MBR.h>
#include <mm/slab.h>

struct vfs_super_block_operations_t fat32_sb_ops;
struct vfs_dir_entry_operations_t fat32_dEntry_ops;
struct vfs_file_operations_t fat32_file_ops;
struct vfs_inode_operations_t fat32_inode_ops;
extern struct blk_gendisk ahci_gendisk0;

static unsigned int vfat_striptail_len(unsigned int len, const char *name);
static int vfat_find(struct vfs_index_node_t *dir, const char *name, struct fat32_slot_info *slot_info);
static int __fat32_search_long_short(struct vfs_index_node_t *parent_inode, const char *name, int name_len,
                                     struct fat32_slot_info *sinfo);
static int fat32_detach_inode(struct vfs_index_node_t *inode);

/**
 * @brief 注册指定磁盘上的指定分区的fat32文件系统
 *
 * @param blk_dev 块设备结构体
 * @param part_num 磁盘分区编号
 *
 * @return struct vfs_super_block_t * 文件系统的超级块
 */
struct vfs_superblock_t *fat32_register_partition(struct block_device *blk_dev, uint8_t part_num)
{
    // 挂载文件系统到vfs
    return vfs_mount_fs("/", "FAT32", blk_dev);
}

/**
 * @brief 计算短目录项文件名的校验和
 *
 * @param name 短目录项文件名字符串（长度为11）
 * @return uint8_t 校验和
 */
static uint8_t fat32_ChkSum(uint8_t *name)
{
    uint8_t chksum = 0;
    for (uint8_t i = 0; i < 11; ++i)
    {
        chksum = ((chksum & 1) ? 0x80 : 0) + (chksum >> 1) + *name;
        ++name;
    }
    return chksum;
}

static int __fat32_search_long_short(struct vfs_index_node_t *parent_inode, const char *name, int name_len,
                                     struct fat32_slot_info *sinfo)
{
    struct fat32_inode_info_t *finode = (struct fat32_inode_info_t *)parent_inode->private_inode_info;
    fat32_sb_info_t *fsbi = (fat32_sb_info_t *)parent_inode->sb->private_sb_info;
    struct block_device *blk = parent_inode->sb->blk_device;

    uint8_t *buf = kzalloc(fsbi->bytes_per_clus, 0);

    // 计算父目录项的起始簇号
    uint32_t cluster = finode->first_clus;

    struct fat32_Directory_t *tmp_dEntry = NULL;
    int cnt_long_dir = 0; // 最终结果中，长目录项的数量

    while (true)
    {

        // 计算父目录项的起始LBA扇区号
        uint64_t sector = __fat32_calculate_LBA(fsbi->first_data_sector, fsbi->sec_per_clus, cluster);
        // kdebug("fat32_part_info[part_id].bootsector.BPB_SecPerClus=%d",fat32_part_info[part_id].bootsector.BPB_SecPerClus);
        // kdebug("sector=%d",sector);

        // 读取父目录项的起始簇数据
        blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_READ_DMA_EXT, sector, fsbi->sec_per_clus, (uint64_t)buf);

        tmp_dEntry = (struct fat32_Directory_t *)buf;

        // 查找每个文件的短目录项
        for (int i = 0; i < fsbi->bytes_per_clus; i += 32, ++tmp_dEntry)
        {
            // 跳过长目录项
            if (tmp_dEntry->DIR_Attr == ATTR_LONG_NAME)
                continue;

            // 跳过无效目录项、空闲目录项
            if (tmp_dEntry->DIR_Name[0] == 0xe5 || tmp_dEntry->DIR_Name[0] == 0x00 || tmp_dEntry->DIR_Name[0] == 0x05)
                continue;
            // kdebug("short name [%d] %s\n 33333==[%#02x]", i / 32, tmp_dEntry->DIR_Name, tmp_dEntry->DIR_Name[3]);
            // 找到长目录项，位于短目录项之前
            struct fat32_LongDirectory_t *tmp_ldEntry = (struct fat32_LongDirectory_t *)tmp_dEntry - 1;
            cnt_long_dir = 0;
            int js = 0;
            // 遍历每个长目录项
            while (tmp_ldEntry->LDIR_Attr == ATTR_LONG_NAME && tmp_ldEntry->LDIR_Ord != 0xe5)
            {
                // 比较name1
                for (int x = 0; x < 5; ++x)
                {
                    if (js >= name_len && (tmp_ldEntry->LDIR_Name1[x] == 0xffff))
                        continue;
                    else if (js > name_len ||
                             tmp_ldEntry->LDIR_Name1[x] != (uint16_t)(name[js++])) // 文件名不匹配，检索下一个短目录项
                        goto continue_cmp_fail;
                }

                // 比较name2
                for (int x = 0; x < 6; ++x)
                {
                    if (js >= name_len && (tmp_ldEntry->LDIR_Name2[x] == 0xffff))
                        continue;
                    else if (js > name_len ||
                             tmp_ldEntry->LDIR_Name2[x] != (uint16_t)(name[js++])) // 文件名不匹配，检索下一个短目录项
                        goto continue_cmp_fail;
                }

                // 比较name3
                for (int x = 0; x < 2; ++x)
                {
                    if (js >= name_len && (tmp_ldEntry->LDIR_Name3[x] == 0xffff))
                        continue;
                    else if (js > name_len ||
                             tmp_ldEntry->LDIR_Name3[x] != (uint16_t)(name[js++])) // 文件名不匹配，检索下一个短目录项
                        goto continue_cmp_fail;
                }

                if (js >= name_len) // 找到需要的目录项，返回
                {
                    // kdebug("found target long name.");
                    cnt_long_dir = tmp_dEntry - (struct fat32_Directory_t *)tmp_ldEntry;
                    goto success;
                }

                --tmp_ldEntry; // 检索下一个长目录项
            }

            // 不存在长目录项，匹配短目录项的基础名
            js = 0;
            for (int x = 0; x < 8; ++x)
            {
                // kdebug("no long name, comparing short name");
                // kdebug("value = %#02x", tmp_dEntry->DIR_Name[x]);
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
                    // ++js;
                    goto continue_cmp_fail;
                    break;
                }
            }
            if (js > name_len)
            {
                // kdebug("js > namelen");
                goto continue_cmp_fail;
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
            if (js > name_len)
            {
                // kdebug("js > namelen");
                goto continue_cmp_fail;
            }
            cnt_long_dir = 0;
            goto success;
        continue_cmp_fail:;
        }

        // 当前簇没有发现目标文件名，寻找下一个簇
        cluster = fat32_read_FAT_entry(blk, fsbi, cluster);

        if (cluster >= 0x0ffffff7) // 寻找完父目录的所有簇，都没有找到目标文件名
        {
            kfree(buf);
            return -ENOENT;
        }
    }
    if (unlikely(tmp_dEntry == NULL))
    {
        BUG_ON(1);
        kfree(buf);
        return -ENOENT;
    }
success:;

    // 填充sinfo
    sinfo->buffer = buf;
    sinfo->de = tmp_dEntry;
    sinfo->i_pos = __fat32_calculate_LBA(fsbi->first_data_sector, fsbi->sec_per_clus, cluster);
    sinfo->num_slots = cnt_long_dir + 1;
    sinfo->slot_off = tmp_dEntry - (struct fat32_Directory_t *)buf;
    // kdebug("successfully found:%s", name);
    return 0;
}

/**
 * @brief 在父目录中寻找指定的目录项
 *
 * @param parent_inode 父目录项的inode
 * @param dest_dentry 搜索目标目录项
 * @return struct vfs_dir_entry_t* 目标目录项
 */
struct vfs_dir_entry_t *fat32_lookup(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dest_dentry)
{
    int errcode = 0;
    fat32_sb_info_t *fsbi = (fat32_sb_info_t *)parent_inode->sb->private_sb_info;
    struct fat32_inode_info_t *finode = NULL;

    struct fat32_slot_info sinfo = {0};
    errcode = vfat_find(parent_inode, dest_dentry->name, &sinfo);

    if (unlikely(errcode != 0))
        return NULL;

find_lookup_success:; // 找到目标dentry
    struct vfs_index_node_t *p = vfs_alloc_inode();

    p->file_size = sinfo.de->DIR_FileSize;
    // 计算文件占用的扇区数, 由于最小存储单位是簇，因此需要按照簇的大小来对齐扇区
    p->blocks = (p->file_size + fsbi->bytes_per_clus - 1) / fsbi->bytes_per_sec;
    p->attribute = (sinfo.de->DIR_Attr & ATTR_DIRECTORY) ? VFS_IF_DIR : VFS_IF_FILE;
    p->sb = parent_inode->sb;
    p->file_ops = &fat32_file_ops;
    p->inode_ops = &fat32_inode_ops;

    // 为inode的与文件系统相关的信息结构体分配空间
    p->private_inode_info = (void *)kzalloc(sizeof(fat32_inode_info_t), 0);
    finode = (fat32_inode_info_t *)p->private_inode_info;

    finode->first_clus = ((sinfo.de->DIR_FstClusHI << 16) | sinfo.de->DIR_FstClusLO) & 0x0fffffff;
    finode->dEntry_location_clus = __fat32_LBA_to_cluster(fsbi->first_data_sector, fsbi->sec_per_clus, sinfo.i_pos);
    finode->dEntry_location_clus_offset = sinfo.slot_off; // 计算dentry的偏移量
    // kdebug("finode->dEntry_location_clus=%#018lx", finode->dEntry_location_clus);
    // kdebug("finode->dEntry_location_clus_offset=%#018lx", finode->dEntry_location_clus_offset);
    finode->create_date = sinfo.de->DIR_CrtDate;
    finode->create_time = sinfo.de->DIR_CrtTime;
    finode->write_date = sinfo.de->DIR_WrtDate;
    finode->write_time = sinfo.de->DIR_WrtTime;

    // 暂时使用fat32的高4bit来标志设备文件
    // todo: 引入devfs后删除这段代码
    if ((sinfo.de->DIR_FstClusHI >> 12) && (p->attribute & VFS_IF_FILE))
        p->attribute |= VFS_IF_DEVICE;

    dest_dentry->dir_inode = p;
    dest_dentry->dir_ops = &fat32_dEntry_ops;
    list_init(&dest_dentry->child_node_list);
    list_init(&dest_dentry->subdirs_list);

    kfree(sinfo.buffer);
    return dest_dentry;
}

/**
 * @brief 创建fat32文件系统的超级块
 *
 * @param blk 块设备结构体
 * @return struct vfs_superblock_t* 创建好的超级块
 */
struct vfs_superblock_t *fat32_read_superblock(struct block_device *blk)
{
    // 读取文件系统的boot扇区
    uint8_t buf[512] = {0};
    blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_READ_DMA_EXT, blk->bd_start_LBA, 1, (uint64_t)&buf);

    // 分配超级块的空间
    struct vfs_superblock_t *sb_ptr = (struct vfs_superblock_t *)kzalloc(sizeof(struct vfs_superblock_t), 0);
    blk->bd_superblock = sb_ptr;
    sb_ptr->sb_ops = &fat32_sb_ops;
    sb_ptr->dir_ops = &fat32_dEntry_ops;
    sb_ptr->private_sb_info = kzalloc(sizeof(fat32_sb_info_t), 0);
    sb_ptr->blk_device = blk;

    struct fat32_BootSector_t *fbs = (struct fat32_BootSector_t *)buf;

    fat32_sb_info_t *fsbi = (fat32_sb_info_t *)(sb_ptr->private_sb_info);

    fsbi->starting_sector = blk->bd_start_LBA;
    fsbi->sector_count = blk->bd_sectors_num;
    fsbi->sec_per_clus = fbs->BPB_SecPerClus;
    fsbi->bytes_per_clus = fbs->BPB_SecPerClus * fbs->BPB_BytesPerSec;
    fsbi->bytes_per_sec = fbs->BPB_BytesPerSec;
    fsbi->first_data_sector = blk->bd_start_LBA + fbs->BPB_RsvdSecCnt + fbs->BPB_FATSz32 * fbs->BPB_NumFATs;
    fsbi->FAT1_base_sector = blk->bd_start_LBA + fbs->BPB_RsvdSecCnt;
    fsbi->FAT2_base_sector = fsbi->FAT1_base_sector + fbs->BPB_FATSz32;
    fsbi->sec_per_FAT = fbs->BPB_FATSz32;
    fsbi->NumFATs = fbs->BPB_NumFATs;
    fsbi->fsinfo_sector_addr_infat = fbs->BPB_FSInfo;
    fsbi->bootsector_bak_sector_addr_infat = fbs->BPB_BkBootSec;

    printk_color(ORANGE, BLACK,
                 "FAT32 Boot Sector\n\tBPB_FSInfo:%#018lx\n\tBPB_BkBootSec:%#018lx\n\tBPB_TotSec32:%#018lx\n",
                 fbs->BPB_FSInfo, fbs->BPB_BkBootSec, fbs->BPB_TotSec32);

    // fsinfo扇区的信息
    memset(&fsbi->fsinfo, 0, sizeof(struct fat32_FSInfo_t));
    blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_READ_DMA_EXT,
                                 blk->bd_start_LBA + fsbi->fsinfo_sector_addr_infat, 1, (uint64_t)&fsbi->fsinfo);

    printk_color(BLUE, BLACK, "FAT32 FSInfo\n\tFSI_LeadSig:%#018lx\n\tFSI_StrucSig:%#018lx\n\tFSI_Free_Count:%#018lx\n",
                 fsbi->fsinfo.FSI_LeadSig, fsbi->fsinfo.FSI_StrucSig, fsbi->fsinfo.FSI_Free_Count);

    // 初始化超级块的dir entry
    sb_ptr->root = vfs_alloc_dentry(2);

    sb_ptr->root->parent = sb_ptr->root;
    sb_ptr->root->dir_ops = &fat32_dEntry_ops;
    // 分配2个字节的name
    sb_ptr->root->name[0] = '/';
    sb_ptr->root->name_length = 1;

    // 为root目录项分配index node
    sb_ptr->root->dir_inode = vfs_alloc_inode();
    sb_ptr->root->dir_inode->inode_ops = &fat32_inode_ops;
    sb_ptr->root->dir_inode->file_ops = &fat32_file_ops;
    sb_ptr->root->dir_inode->file_size = 0;
    // 计算文件占用的扇区数, 由于最小存储单位是簇，因此需要按照簇的大小来对齐扇区
    sb_ptr->root->dir_inode->blocks =
        (sb_ptr->root->dir_inode->file_size + fsbi->bytes_per_clus - 1) / fsbi->bytes_per_sec;
    sb_ptr->root->dir_inode->attribute = VFS_IF_DIR;
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

    struct fat32_Directory_t *buf = (struct fat32_Directory_t *)kmalloc(fsbi->bytes_per_clus, 0);
    memset(buf, 0, fsbi->bytes_per_clus);

    inode->sb->blk_device->bd_disk->fops->transfer(inode->sb->blk_device->bd_disk, AHCI_CMD_READ_DMA_EXT, fLBA,
                                                   fsbi->sec_per_clus, (uint64_t)buf);
    // 计算目标dEntry所在的位置
    struct fat32_Directory_t *fdEntry = buf + finode->dEntry_location_clus_offset;

    // 写入fat32文件系统的dir_entry
    fdEntry->DIR_FileSize = inode->file_size;
    fdEntry->DIR_FstClusLO = finode->first_clus & 0xffff;
    fdEntry->DIR_FstClusHI = (finode->first_clus >> 16) | (fdEntry->DIR_FstClusHI & 0xf000);

    // 将dir entry写回磁盘
    inode->sb->blk_device->bd_disk->fops->transfer(inode->sb->blk_device->bd_disk, AHCI_CMD_WRITE_DMA_EXT, fLBA,
                                                   fsbi->sec_per_clus, (uint64_t)buf);
    kfree(buf);
}

struct vfs_super_block_operations_t fat32_sb_ops = {
    .write_superblock = fat32_write_superblock,
    .put_superblock = fat32_put_superblock,
    .write_inode = fat32_write_inode,
};

// todo: compare
long fat32_compare(struct vfs_dir_entry_t *parent_dEntry, char *source_filename, char *dest_filename)
{
    return 0;
}
// todo: hash
long fat32_hash(struct vfs_dir_entry_t *dEntry, char *filename)
{
    return 0;
}
// todo: release
long fat32_release(struct vfs_dir_entry_t *dEntry)
{
    return 0;
}
// todo: iput
long fat32_iput(struct vfs_dir_entry_t *dEntry, struct vfs_index_node_t *inode)
{
    return 0;
}

/**
 * @brief fat32文件系统对于dEntry的操作
 *
 */
struct vfs_dir_entry_operations_t fat32_dEntry_ops = {
    .compare = fat32_compare,
    .hash = fat32_hash,
    .release = fat32_release,
    .iput = fat32_iput,
};

// todo: open
long fat32_open(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr)
{
    return 0;
}

// todo: close
long fat32_close(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr)
{
    return 0;
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
    struct block_device *blk = file_ptr->dEntry->dir_inode->sb->blk_device;

    // First cluster num of the file
    uint64_t cluster = finode->first_clus;
    // kdebug("fsbi->bytes_per_clus=%d fsbi->sec_per_clus=%d finode->first_clus=%d cluster=%d", fsbi->bytes_per_clus,
    // fsbi->sec_per_clus, finode->first_clus, cluster);

    // kdebug("fsbi->bytes_per_clus=%d", fsbi->bytes_per_clus);

    // clus offset in file
    uint64_t clus_offset_in_file = (*position) / fsbi->bytes_per_clus;
    // bytes offset in clus
    uint64_t bytes_offset = (*position) % fsbi->bytes_per_clus;

    if (!cluster)
        return -EFAULT;

    // find the actual cluster on disk of the specified position
    for (int i = 0; i < clus_offset_in_file; ++i)
        cluster = fat32_read_FAT_entry(blk, fsbi, cluster);

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
        int errno = blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_READ_DMA_EXT, sector, fsbi->sec_per_clus,
                                                 (uint64_t)tmp_buffer);
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

        cluster = fat32_read_FAT_entry(blk, fsbi, cluster);
    } while (bytes_remain && (cluster < 0x0ffffff8) && cluster != 0);

    kfree(tmp_buffer);

    if (!bytes_remain)
        retval = count;

    return retval;
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
    struct block_device *blk = file_ptr->dEntry->dir_inode->sb->blk_device;

    // First cluster num of the file
    uint32_t cluster = finode->first_clus;
    int64_t flags = 0;

    // clus offset in file
    uint64_t clus_offset_in_file = (*position) / fsbi->bytes_per_clus;
    // bytes offset in clus
    uint64_t bytes_offset = (*position) % fsbi->bytes_per_clus;

    if (!cluster) // 起始簇号为0，说明是空文件
    {
        // 分配空闲簇
        if (fat32_alloc_clusters(file_ptr->dEntry->dir_inode, &cluster, 1) != 0)
            return -ENOSPC;
    }
    else
    {
        // 跳转到position所在的簇
        for (uint64_t i = 0; i < clus_offset_in_file; ++i)
            cluster = fat32_read_FAT_entry(blk, fsbi, cluster);
    }
    // kdebug("cluster(start)=%d", cluster);
    //  没有可用的磁盘空间
    if (!cluster)
        return -ENOSPC;

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
            int errno = blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_READ_DMA_EXT, sector, fsbi->sec_per_clus,
                                                     (uint64_t)tmp_buffer);
            if (errno != 0)
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
        int errno = blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_WRITE_DMA_EXT, sector, fsbi->sec_per_clus,
                                                 (uint64_t)tmp_buffer);
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
            next_clus = fat32_read_FAT_entry(blk, fsbi, cluster);
        else
            break;
        if (next_clus >= 0x0ffffff8) // 已经到达了最后一个簇，需要分配新簇
        {
            if (fat32_alloc_clusters(file_ptr->dEntry->dir_inode, &next_clus, 1) != 0)
            {
                // 没有空闲簇
                kfree(tmp_buffer);
                return -ENOSPC;
            }

            cluster = next_clus; // 切换当前簇
            flags = 1;           // 标记当前簇是新分配的簇
        }

    } while (bytes_remain);

    // 文件大小有增长
    if (*position > (file_ptr->dEntry->dir_inode->file_size))
    {
        file_ptr->dEntry->dir_inode->file_size = *position;
        file_ptr->dEntry->dir_inode->sb->sb_ops->write_inode(file_ptr->dEntry->dir_inode);
        // kdebug("new file size=%ld", *position);
    }

    kfree(tmp_buffer);
    if (!bytes_remain)
        retval = count;
    // kdebug("retval=%lld", retval);
    return retval;
}

/**
 * @brief 调整文件的当前访问位置
 *
 * @param file_ptr vfs文件指针
 * @param offset 调整的偏移量
 * @param whence 调整方法
 * @return long 更新后的指针位置
 */
long fat32_lseek(struct vfs_file_t *file_ptr, long offset, long whence)
{
    struct vfs_index_node_t *inode = file_ptr->dEntry->dir_inode;

    long pos = 0;
    switch (whence)
    {
    case SEEK_SET: // 相对于文件头
        pos = offset;
        break;
    case SEEK_CUR: // 相对于当前位置
        pos = file_ptr->position + offset;
        break;
    case SEEK_END: // 相对于文件末尾
        pos = file_ptr->dEntry->dir_inode->file_size + offset;
        break;

    default:
        return -EINVAL;
        break;
    }

    if (pos < 0 || pos > file_ptr->dEntry->dir_inode->file_size)
        return -EOVERFLOW;
    file_ptr->position = pos;

    // kdebug("fat32 lseek -> position=%d", file_ptr->position);
    return pos;
}
// todo: ioctl
long fat32_ioctl(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr, uint64_t cmd, uint64_t arg)
{
    return 0;
}

/**
 * @brief fat32文件系统，关于文件的操作
 *
 */
struct vfs_file_operations_t fat32_file_ops = {
    .open = fat32_open,
    .close = fat32_close,
    .read = fat32_read,
    .write = fat32_write,
    .lseek = fat32_lseek,
    .ioctl = fat32_ioctl,
    .readdir = fat32_readdir,
};

/**
 * @brief 创建新的文件
 * @param parent_inode 父目录的inode结构体
 * @param dest_dEntry 新文件的dentry
 * @param mode 创建模式
 */
long fat32_create(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dest_dEntry, int mode)
{
    // 文件系统超级块信息
    fat32_sb_info_t *fsbi = (fat32_sb_info_t *)parent_inode->sb->private_sb_info;
    // 父目录项的inode的私有信息
    struct fat32_inode_info_t *parent_inode_info = (struct fat32_inode_info_t *)parent_inode->private_inode_info;

    int64_t retval = 0;

    // ======== 检验名称的合法性
    retval = fat32_check_name_available(dest_dEntry->name, dest_dEntry->name_length, 0);

    if (retval != 0)
        return retval;

    if (dest_dEntry->dir_inode != NULL)
        return -EEXIST;

    struct vfs_index_node_t *inode = vfs_alloc_inode();
    dest_dEntry->dir_inode = inode;
    dest_dEntry->dir_ops = &fat32_dEntry_ops;

    struct fat32_inode_info_t *finode = (struct fat32_inode_info_t *)kzalloc(sizeof(struct fat32_inode_info_t), 0);
    inode->attribute = VFS_IF_FILE;
    inode->file_ops = &fat32_file_ops;
    inode->file_size = 0;
    inode->sb = parent_inode->sb;
    inode->inode_ops = &fat32_inode_ops;
    inode->private_inode_info = (void *)finode;
    inode->blocks = fsbi->sec_per_clus;

    struct block_device *blk = inode->sb->blk_device;

    // 计算总共需要多少个目录项
    uint32_t cnt_longname = (dest_dEntry->name_length + 25) / 26;
    // 默认都是创建长目录项来存储
    if (cnt_longname == 0)
        cnt_longname = 1;

    // 空闲dentry所在的扇区号
    uint32_t tmp_dentry_sector = 0;
    // 空闲dentry所在的缓冲区的基地址
    uint64_t tmp_dentry_clus_buf_addr = 0;
    uint64_t tmp_parent_dentry_clus = 0;
    // 寻找空闲目录项
    struct fat32_Directory_t *empty_fat32_dentry = fat32_find_empty_dentry(
        parent_inode, cnt_longname + 1, 0, &tmp_dentry_sector, &tmp_parent_dentry_clus, &tmp_dentry_clus_buf_addr);
    // kdebug("found empty dentry, cnt_longname=%ld", cnt_longname);

    finode->first_clus = 0;
    finode->dEntry_location_clus = tmp_parent_dentry_clus;
    finode->dEntry_location_clus_offset = empty_fat32_dentry - (struct fat32_Directory_t *)tmp_dentry_clus_buf_addr;

    // ====== 为新的文件分配一个簇 =======
    uint32_t new_dir_clus;
    if (fat32_alloc_clusters(inode, &new_dir_clus, 1) != 0)
    {
        retval = -ENOSPC;
        goto fail;
    }
    // kdebug("new dir clus=%ld", new_dir_clus);
    // kdebug("dest_dEntry->name=%s", dest_dEntry->name);

    // ====== 填写短目录项
    fat32_fill_shortname(dest_dEntry, empty_fat32_dentry, new_dir_clus);
    // kdebug("dest_dEntry->name=%s",dest_dEntry->name);

    // 计算校验和
    uint8_t short_dentry_ChkSum = fat32_ChkSum(empty_fat32_dentry->DIR_Name);

    // kdebug("dest_dEntry->name=%s", dest_dEntry->name);
    // ======== 填写长目录项
    fat32_fill_longname(dest_dEntry, (struct fat32_LongDirectory_t *)(empty_fat32_dentry - 1), short_dentry_ChkSum,
                        cnt_longname);

    // ====== 将目录项写回磁盘
    // kdebug("tmp_dentry_sector=%ld", tmp_dentry_sector);
    blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_WRITE_DMA_EXT, tmp_dentry_sector, fsbi->sec_per_clus,
                                 tmp_dentry_clus_buf_addr);

    // 注意：parent字段需要在调用函数的地方进行设置

    // 释放在find empty dentry中动态申请的缓冲区
    kfree((void *)tmp_dentry_clus_buf_addr);
    return 0;
fail:;
    // 释放在find empty dentry中动态申请的缓冲区
    kfree((void *)tmp_dentry_clus_buf_addr);
    dest_dEntry->dir_inode = NULL;
    dest_dEntry->dir_ops = NULL;
    kfree(finode);
    kfree(inode);
    return retval;
}

/**
 * @brief 创建文件夹
 * @param inode 父目录的inode
 * @param dEntry 新的文件夹的dentry
 * @param mode 创建文件夹的mode
 * @return long 错误码
 */
int64_t fat32_mkdir(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dEntry, int mode)
{
    int64_t retval = 0;

    // 文件系统超级块信息
    fat32_sb_info_t *fsbi = (fat32_sb_info_t *)parent_inode->sb->private_sb_info;
    // 父目录项的inode私有信息
    struct fat32_inode_info_t *parent_inode_info = (struct fat32_inode_info_t *)parent_inode->private_inode_info;
    // ======== 检验名称的合法性
    retval = fat32_check_name_available(dEntry->name, dEntry->name_length, 0);
    if (retval != 0)
        return retval;
    // ====== 找一块连续的区域放置新的目录项 =====

    // 计算总共需要多少个目录项
    uint32_t cnt_longname = (dEntry->name_length + 25) / 26;
    // 默认都是创建长目录项来存储
    if (cnt_longname == 0)
        cnt_longname = 1;

    // 空闲dentry所在的扇区号
    uint32_t tmp_dentry_sector = 0;
    // 空闲dentry所在的缓冲区的基地址
    uint64_t tmp_dentry_clus_buf_addr = 0;
    uint64_t tmp_parent_dentry_clus = 0;
    // 寻找空闲目录项
    struct fat32_Directory_t *empty_fat32_dentry = fat32_find_empty_dentry(
        parent_inode, cnt_longname + 1, 0, &tmp_dentry_sector, &tmp_parent_dentry_clus, &tmp_dentry_clus_buf_addr);

    // ====== 初始化inode =======
    struct vfs_index_node_t *inode = vfs_alloc_inode();
    inode->attribute = VFS_IF_DIR;
    inode->blocks = fsbi->sec_per_clus;
    inode->file_ops = &fat32_file_ops;
    inode->file_size = 0;
    inode->inode_ops = &fat32_inode_ops;
    inode->sb = parent_inode->sb;

    struct block_device *blk = inode->sb->blk_device;

    // ===== 初始化inode的文件系统私有信息 ====

    inode->private_inode_info = (fat32_inode_info_t *)kmalloc(sizeof(fat32_inode_info_t), 0);
    memset(inode->private_inode_info, 0, sizeof(fat32_inode_info_t));
    fat32_inode_info_t *p = (fat32_inode_info_t *)inode->private_inode_info;
    p->first_clus = 0;
    p->dEntry_location_clus = tmp_parent_dentry_clus;
    p->dEntry_location_clus_offset = empty_fat32_dentry - (struct fat32_Directory_t *)tmp_dentry_clus_buf_addr;
    // kdebug(" p->dEntry_location_clus_offset=%d", p->dEntry_location_clus_offset);
    // todo: 填写完全fat32_inode_info的信息

    // 初始化dentry信息
    dEntry->dir_ops = &fat32_dEntry_ops;
    dEntry->dir_inode = inode;

    // ====== 为新的文件夹分配一个簇 =======
    uint32_t new_dir_clus;
    if (fat32_alloc_clusters(inode, &new_dir_clus, 1) != 0)
    {
        retval = -ENOSPC;
        goto fail;
    }

    // kdebug("new dir clus=%ld", new_dir_clus);

    // ====== 填写短目录项
    fat32_fill_shortname(dEntry, empty_fat32_dentry, new_dir_clus);

    // 计算校验和
    uint8_t short_dentry_ChkSum = fat32_ChkSum(empty_fat32_dentry->DIR_Name);

    // ======== 填写长目录项
    fat32_fill_longname(dEntry, (struct fat32_LongDirectory_t *)(empty_fat32_dentry - 1), short_dentry_ChkSum,
                        cnt_longname);

    // ====== 将目录项写回磁盘
    // kdebug("tmp_dentry_sector=%ld", tmp_dentry_sector);
    blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_WRITE_DMA_EXT, tmp_dentry_sector, fsbi->sec_per_clus,
                                 tmp_dentry_clus_buf_addr);
    // ====== 初始化新的文件夹的目录项 =====
    {
        // kdebug("to create dot and dot dot.");
        void *buf = kmalloc(fsbi->bytes_per_clus, 0);
        struct fat32_Directory_t *new_dir_dentries = (struct fat32_Directory_t *)buf;
        memset((void *)new_dir_dentries, 0, fsbi->bytes_per_clus);

        // 新增 . 目录项
        new_dir_dentries->DIR_Attr = ATTR_DIRECTORY;
        new_dir_dentries->DIR_FileSize = 0;
        new_dir_dentries->DIR_Name[0] = '.';
        for (int i = 1; i < 11; ++i)
            new_dir_dentries->DIR_Name[i] = 0x20;

        new_dir_dentries->DIR_FstClusHI = empty_fat32_dentry->DIR_FstClusHI;
        new_dir_dentries->DIR_FstClusLO = empty_fat32_dentry->DIR_FstClusLO;

        // 新增 .. 目录项
        ++new_dir_dentries;
        new_dir_dentries->DIR_Attr = ATTR_DIRECTORY;
        new_dir_dentries->DIR_FileSize = 0;
        new_dir_dentries->DIR_Name[0] = '.';
        new_dir_dentries->DIR_Name[1] = '.';
        for (int i = 2; i < 11; ++i)
            new_dir_dentries->DIR_Name[i] = 0x20;
        new_dir_dentries->DIR_FstClusHI = (unsigned short)(parent_inode_info->first_clus >> 16) & 0x0fff;
        new_dir_dentries->DIR_FstClusLO = (unsigned short)(parent_inode_info->first_clus) & 0xffff;

        // 写入磁盘

        uint64_t sector = fsbi->first_data_sector + (new_dir_clus - 2) * fsbi->sec_per_clus;
        // kdebug("add dot and dot dot: sector=%ld", sector);
        blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_WRITE_DMA_EXT, sector, fsbi->sec_per_clus, (uint64_t)buf);
    }

    // 注意：parent字段需要在调用函数的地方进行设置
    // 注意：需要将当前dentry加入父目录的subdirs_list

    // 释放在find empty dentry中动态申请的缓冲区
    kfree((void *)tmp_dentry_clus_buf_addr);

    return 0;
fail:;
    // 释放在find empty dentry中动态申请的缓冲区
    kfree((void *)tmp_dentry_clus_buf_addr);
    return retval;
}

// todo: rmdir
int64_t fat32_rmdir(struct vfs_index_node_t *inode, struct vfs_dir_entry_t *dEntry)
{
    return 0;
}

// todo: rename
int64_t fat32_rename(struct vfs_index_node_t *old_inode, struct vfs_dir_entry_t *old_dEntry,
                     struct vfs_index_node_t *new_inode, struct vfs_dir_entry_t *new_dEntry)
{
    return 0;
}

// todo: getAttr
int64_t fat32_getAttr(struct vfs_dir_entry_t *dEntry, uint64_t *attr)
{
    return 0;
}

// todo: setAttr
int64_t fat32_setAttr(struct vfs_dir_entry_t *dEntry, uint64_t *attr)
{
    return 0;
}

/**
 * @brief 从fat32中卸载inode
 *
 * @param inode 要被卸载的inode
 * @return int 错误码
 */
static int fat32_detach_inode(struct vfs_index_node_t *inode)
{
    // todo: 当引入哈希表管理inode之后，这个函数负责将inode从哈希表中删除
    // 参考Linux的fat_detach
    return 0;
}

/**
 * @brief 取消inode和dentry之间的链接关系（删除文件）
 *
 * @param inode 要被取消关联关系的目录项的【父目录项】
 * @param dentry 要被取消关联关系的子目录项
 */
int64_t fat32_unlink(struct vfs_index_node_t *dir, struct vfs_dir_entry_t *dentry)
{
    int retval = 0;
    struct vfs_superblock_t *sb = dir->sb;
    struct vfs_index_node_t *inode_to_remove = dentry->dir_inode;
    fat32_sb_info_t *fsbi = (fat32_sb_info_t *)sb->private_sb_info;
    struct fat32_slot_info sinfo = {0};
    // todo: 对fat32的超级块加锁

    retval = vfat_find(dir, dentry->name, &sinfo);

    if (unlikely(retval != 0))
        goto out;

    // 从fat表删除目录项
    retval = fat32_remove_entries(dir, &sinfo);
    if (unlikely(retval != 0))
        goto out;
    retval = fat32_detach_inode(dentry->dir_inode);
    if (unlikely(retval != 0))
        goto out;
out:;
    if (sinfo.buffer != NULL)
        kfree(sinfo.buffer);
    // todo: 对fat32的超级块放锁
    return retval;
}

/**
 * @brief 读取文件夹(在指定目录中找出有效目录项)
 *
 * @param file_ptr 文件结构体指针
 * @param dirent 返回的dirent
 * @param filler 填充dirent的函数
 * @return uint64_t dirent的总大小
 */
int64_t fat32_readdir(struct vfs_file_t *file_ptr, void *dirent, vfs_filldir_t filler)
{
    struct fat32_inode_info_t *finode = (struct fat32_inode_info_t *)file_ptr->dEntry->dir_inode->private_inode_info;
    fat32_sb_info_t *fsbi = (fat32_sb_info_t *)file_ptr->dEntry->dir_inode->sb->private_sb_info;
    struct block_device *blk = file_ptr->dEntry->dir_inode->sb->blk_device;

    unsigned char *buf = (unsigned char *)kzalloc(fsbi->bytes_per_clus, 0);
    uint32_t cluster = finode->first_clus;

    // 当前文件指针所在位置的簇号（文件内偏移量）
    int clus_num = file_ptr->position / fsbi->bytes_per_clus;

    // 循环读取fat entry，直到读取到文件当前位置的所在簇号
    for (int i = 0; i < clus_num; ++i)
    {
        cluster = fat32_read_FAT_entry(blk, fsbi, cluster);
        if (cluster > 0x0ffffff7) // 文件结尾
        {
            kerror("file position out of range! (cluster not exists)");
            return NULL;
        }
    }

    uint64_t dentry_type = 0; // 传递给filler的dentry类型数据

    char *dir_name = NULL;
    int name_len = 0;
    // ==== 此时已经将文件夹的目录项起始簇的簇号读取到cluster变量中 ===
    while (cluster <= 0x0ffffff7) // cluster在循环末尾更新（如果当前簇已经没有短目录项的话）
    {
        // 计算文件夹当前位置所在簇的起始扇区号
        uint64_t sector = fsbi->first_data_sector + (cluster - 2) * fsbi->sec_per_clus;
        // 读取文件夹目录项当前位置起始扇区的数据

        if (AHCI_SUCCESS != blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_READ_DMA_EXT, sector,
                                                         fsbi->sec_per_clus, (uint64_t)buf))
        {
            // 读取失败
            kerror("Failed to read the file's first sector.");
            kfree(buf);
            return NULL;
        }

        struct fat32_Directory_t *dentry = NULL;
        struct fat32_LongDirectory_t *long_dentry = NULL;

        // 找到当前短目录项
        dentry = (struct fat32_Directory_t *)(buf + file_ptr->position % fsbi->bytes_per_clus);

        name_len = 0;
        // 逐个查找短目录项
        for (int i = file_ptr->position % fsbi->bytes_per_clus; i < fsbi->bytes_per_clus;
             i += 32, file_ptr->position += 32, ++dentry)
        {
            // 若是长目录项则跳过
            if (dentry->DIR_Attr == ATTR_LONG_NAME)
                continue;
            // 跳过无效表项、空闲表项
            if (dentry->DIR_Name[0] == 0xe5 || dentry->DIR_Name[0] == 0x00 || dentry->DIR_Name[0] == 0x05)
                continue;

            // 找到短目录项
            // 该短目录项对应的第一个长目录项
            long_dentry = (struct fat32_LongDirectory_t *)(dentry - 1);

            // 如果长目录项有效，则读取长目录项
            if (long_dentry->LDIR_Attr == ATTR_LONG_NAME && long_dentry->LDIR_Ord != 0xe5 &&
                long_dentry->LDIR_Ord != 0x00 && long_dentry->LDIR_Ord != 0x05)
            {
                int count_long_dentry = 0;
                // 统计长目录项的个数
                while (long_dentry->LDIR_Attr == ATTR_LONG_NAME && long_dentry->LDIR_Ord != 0xe5 &&
                       long_dentry->LDIR_Ord != 0x00 && long_dentry->LDIR_Ord != 0x05)
                {
                    ++count_long_dentry;
                    if (long_dentry->LDIR_Ord & 0x40) // 最后一个长目录项
                        break;
                    --long_dentry;
                }
                // 为目录名分配空间
                dir_name = (char *)kmalloc(count_long_dentry * 26 + 1, 0);
                memset(dir_name, 0, count_long_dentry * 26 + 1);

                // 重新将长目录项指针指向第一个长目录项
                long_dentry = (struct fat32_LongDirectory_t *)(dentry - 1);
                name_len = 0;
                // 逐个存储文件名
                for (int j = 0; j < count_long_dentry; ++j, --long_dentry)
                {
                    // 存储name1
                    for (int k = 0; k < 5; ++k)
                    {
                        if (long_dentry->LDIR_Name1[k] != 0xffff && long_dentry->LDIR_Name1[k] != 0x0000)
                            dir_name[name_len++] = (char)long_dentry->LDIR_Name1[k];
                    }

                    // 存储name2
                    for (int k = 0; k < 6; ++k)
                    {
                        if (long_dentry->LDIR_Name2[k] != 0xffff && long_dentry->LDIR_Name2[k] != 0x0000)
                            dir_name[name_len++] = (char)long_dentry->LDIR_Name2[k];
                    }

                    // 存储name3
                    for (int k = 0; k < 2; ++k)
                    {
                        if (long_dentry->LDIR_Name3[k] != 0xffff && long_dentry->LDIR_Name3[k] != 0x0000)
                            dir_name[name_len++] = (char)long_dentry->LDIR_Name3[k];
                    }
                }

                // 读取目录项成功，返回
                dentry_type = dentry->DIR_Attr;
                goto find_dir_success;
            }
            else // 不存在长目录项
            {
                dir_name = (char *)kmalloc(15, 0);
                memset(dir_name, 0, 15);

                name_len = 0;
                int total_len = 0;
                // 读取基础名
                for (int j = 0; j < 8; ++j, ++total_len)
                {
                    if (dentry->DIR_Name[j] == ' ')
                        break;

                    if (dentry->DIR_NTRes & LOWERCASE_BASE) // 如果标记了文件名小写，则转换为小写字符
                        dir_name[name_len++] = dentry->DIR_Name[j] + 32;
                    else
                        dir_name[name_len++] = dentry->DIR_Name[j];
                }

                // 如果当前短目录项为文件夹，则直接返回，不需要读取扩展名
                if (dentry->DIR_Attr & ATTR_DIRECTORY)
                {
                    dentry_type = dentry->DIR_Attr;
                    goto find_dir_success;
                }

                // 是文件，增加  .
                dir_name[name_len++] = '.';

                // 读取扩展名
                // 读取基础名
                for (int j = 0; j < 3; ++j, ++total_len)
                {
                    if (dentry->DIR_Name[j] == ' ')
                        break;

                    if (dentry->DIR_NTRes & LOWERCASE_BASE) // 如果标记了文件名小写，则转换为小写字符
                        dir_name[name_len++] = dentry->DIR_Name[j] + 32;
                    else
                        dir_name[name_len++] = dentry->DIR_Name[j];
                }

                if (total_len == 8) // 没有扩展名
                    dir_name[--name_len] = '\0';

                dentry_type = dentry->DIR_Attr;
                goto find_dir_success;
            }
        }

        // 当前簇不存在目录项
        cluster = fat32_read_FAT_entry(blk, fsbi, cluster);
    }

    kfree(buf);
    // 在上面的循环中读取到目录项结尾了，仍没有找到
    return NULL;

find_dir_success:;
    // 将文件夹位置坐标加32（即指向下一个目录项）
    file_ptr->position += 32;
    // todo: 计算ino_t
    if (dentry_type & ATTR_DIRECTORY)
        dentry_type = VFS_IF_DIR;
    else
        dentry_type = VFS_IF_FILE;

    return filler(dirent, 0, dir_name, name_len, dentry_type, 0);
}

struct vfs_inode_operations_t fat32_inode_ops = {
    .create = fat32_create,
    .mkdir = fat32_mkdir,
    .rmdir = fat32_rmdir,
    .lookup = fat32_lookup,
    .rename = fat32_rename,
    .getAttr = fat32_getAttr,
    .setAttr = fat32_setAttr,
    .unlink = fat32_unlink,

};

/**
 * @brief 给定字符串长度，计算去除字符串尾部的'.'后，剩余部分的长度
 *
 * @param len 字符串长度（不包括\0）
 * @param name 名称字符串
 * @return unsigned int 去除'.'后的
 */
static unsigned int vfat_striptail_len(unsigned int len, const char *name)
{
    while (len && name[len - 1] == '.')
        --len;
    return len;
}

/**
 * @brief 在指定inode的长目录项中搜索目标子目录项
 *
 * @param dir 父目录项inode
 * @param name 要查找的子目录项的名称
 * @param len 子目录项名称长度
 * @param slot_info 返回的对应的子目录项的短目录项。
 * @return int 错误码
 */
static int fat_search_long(struct vfs_index_node_t *dir, const char *name, int len, struct fat32_slot_info *slot_info)
{
    int retval = 0;
    retval = __fat32_search_long_short(dir, name, len, slot_info);
    return retval;
}
/**
 * @brief 在fat32中，根据父inode,寻找给定名称的子inode
 *
 * @param dir 父目录项的inode
 * @param name 子目录项名称
 * @param slot_info 找到的slot的信息
 * @return int 错误码
 */
static int vfat_find(struct vfs_index_node_t *dir, const char *name, struct fat32_slot_info *slot_info)
{
    uint32_t len = vfat_striptail_len(strnlen(name, PAGE_4K_SIZE - 1), name);

    if (len == 0)
        return -ENOENT;

    return fat_search_long(dir, name, len, slot_info);
}

struct vfs_filesystem_type_t fat32_fs_type = {
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
    fat32_register_partition(ahci_gendisk0.partition + 0, 0);
    kinfo("FAT32 initialized.");
}