#include "fat_ent.h"
#include "internal.h"
#include <common/errno.h>
#include <driver/disk/ahci/ahci.h>
#include <mm/slab.h>

static const char unavailable_character_in_short_name[] = {0x22, 0x2a, 0x2b, 0x2c, 0x2e, 0x2f, 0x3a, 0x3b,
                                                           0x3c, 0x3d, 0x3e, 0x3f, 0x5b, 0x5c, 0x5d, 0x7c};
/**
 * @brief 请求分配指定数量的簇
 *
 * @param inode 要分配簇的inode
 * @param clusters 返回的被分配的簇的簇号结构体
 * @param num_clusters 要分配的簇的数量
 * @return int 错误码
 */
int fat32_alloc_clusters(struct vfs_index_node_t *inode, uint32_t *clusters, int32_t num_clusters)
{
    int retval = 0;

    fat32_sb_info_t *fsbi = (fat32_sb_info_t *)inode->sb->private_sb_info;
    struct fat32_inode_info_t *finode = (struct fat32_inode_info_t *)inode->private_inode_info;
    struct block_device *blk = inode->sb->blk_device;
    uint64_t sec_per_fat = fsbi->sec_per_FAT;

    // 申请1扇区的缓冲区
    uint32_t *buf = (uint32_t *)kzalloc(fsbi->bytes_per_sec, 0);
    int ent_per_sec = (fsbi->bytes_per_sec >> 2);
    int clus_idx = 0;
    for (int i = 0; i < sec_per_fat; ++i)
    {
        if (clus_idx >= num_clusters)
            goto done;
        memset(buf, 0, fsbi->bytes_per_sec);
        blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_READ_DMA_EXT, fsbi->FAT1_base_sector + i, 1, (uint64_t)buf);
        // 依次检查簇是否空闲
        for (int j = 0; j < ent_per_sec; ++j)
        {
            if (clus_idx >= num_clusters)
                goto done;
            // 找到空闲簇
            if ((buf[j] & 0x0fffffff) == 0)
            {
                // kdebug("clus[%d] = %d", clus_idx, i * ent_per_sec + j);
                clusters[clus_idx] = i * ent_per_sec + j;
                ++clus_idx;
            }
        }
    }
    // 空间不足
    retval = -ENOSPC;

done:;
    kfree(buf);
    if (retval == 0) // 成功
    {
        int cluster, idx;
        if (finode->first_clus == 0)
        {
            // 空文件
            finode->first_clus = clusters[0];
            cluster = finode->first_clus;
            // 写入inode到磁盘
            inode->sb->sb_ops->write_inode(inode);
            idx = 1;
        }
        else
        {
            // 跳转到文件当前的最后一个簇
            idx = 0;
            int tmp_clus = finode->first_clus;
            cluster = tmp_clus;
            while (true)
            {
                tmp_clus = fat32_read_FAT_entry(blk, fsbi, cluster);
                if (tmp_clus <= 0x0ffffff7)
                    cluster = tmp_clus;
                else
                    break;
            }
        }

        // 写入fat表
        for (int i = idx; i < num_clusters; ++i)
        {
            // kdebug("write cluster i=%d : cluster=%d, value= %d", i, cluster, clusters[i]);
            fat32_write_FAT_entry(blk, fsbi, cluster, clusters[i]);
            cluster = clusters[i];
        }
        fat32_write_FAT_entry(blk, fsbi, cluster, 0x0ffffff8);

        return 0;
    }
    else // 出现错误
    {
        kwarn("err in alloc clusters");
        if (clus_idx < num_clusters)
            fat32_free_clusters(inode, clusters[0]);
        return retval;
    }

    return 0;
}

/**
 * @brief 释放从属于inode的，从cluster开始的所有簇
 *
 * @param inode 指定的文件的inode
 * @param cluster 指定簇
 * @return int 错误码
 */
int fat32_free_clusters(struct vfs_index_node_t *inode, int32_t cluster)
{
    // todo: 释放簇
    return 0;
}

/**
 * @brief 读取指定簇的FAT表项
 *
 * @param blk 块设备结构体
 * @param fsbi fat32超级块私有信息结构体
 * @param cluster 指定簇
 * @return uint32_t 下一个簇的簇号
 */
uint32_t fat32_read_FAT_entry(struct block_device *blk, fat32_sb_info_t *fsbi, uint32_t cluster)
{
    // 计算每个扇区内含有的FAT表项数
    // FAT每项4bytes
    uint32_t fat_ent_per_sec = (fsbi->bytes_per_sec >> 2); // 该值应为2的n次幂

    uint32_t buf[256];
    memset(buf, 0, fsbi->bytes_per_sec);

    // 读取一个sector的数据，
    blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_READ_DMA_EXT,
                                 fsbi->FAT1_base_sector + (cluster / fat_ent_per_sec), 1, (uint64_t)&buf);

    // 返回下一个fat表项的值（也就是下一个cluster）
    return buf[cluster & (fat_ent_per_sec - 1)] & 0x0fffffff;
}

/**
 * @brief 写入指定簇的FAT表项
 *
 * @param blk 块设备结构体
 * @param fsbi fat32超级块私有信息结构体
 * @param cluster 指定簇
 * @param value 要写入该fat表项的值
 * @return uint32_t errcode
 */
int fat32_write_FAT_entry(struct block_device *blk, fat32_sb_info_t *fsbi, uint32_t cluster, uint32_t value)
{
    // 计算每个扇区内含有的FAT表项数
    // FAT每项4bytes
    uint32_t fat_ent_per_sec = (fsbi->bytes_per_sec >> 2); // 该值应为2的n次幂
    uint32_t *buf = kzalloc(fsbi->bytes_per_sec, 0);

    blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_READ_DMA_EXT,
                                 fsbi->FAT1_base_sector + (cluster / fat_ent_per_sec), 1, (uint64_t)buf);

    buf[cluster & (fat_ent_per_sec - 1)] = (buf[cluster & (fat_ent_per_sec - 1)] & 0xf0000000) | (value & 0x0fffffff);
    // 向FAT1和FAT2写入数据
    blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_WRITE_DMA_EXT,
                                 fsbi->FAT1_base_sector + (cluster / fat_ent_per_sec), 1, (uint64_t)buf);
    blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_WRITE_DMA_EXT,
                                 fsbi->FAT2_base_sector + (cluster / fat_ent_per_sec), 1, (uint64_t)buf);

    kfree(buf);
    return 0;
}

/**
 * @brief 在父亲inode的目录项簇中，寻找连续num个空的目录项
 *
 * @param parent_inode 父inode
 * @param num 请求的目录项数量
 * @param mode 操作模式
 * @param res_sector 返回信息：缓冲区对应的扇区号
 * @param res_cluster 返回信息：缓冲区对应的簇号
 * @param res_data_buf_base 返回信息：缓冲区的内存基地址（记得要释放缓冲区内存！！！！）
 * @return struct fat32_Directory_t*
 * 符合要求的entry的指针（指向地址高处的空目录项，也就是说，有连续num个≤这个指针的空目录项）
 */
struct fat32_Directory_t *fat32_find_empty_dentry(struct vfs_index_node_t *parent_inode, uint32_t num, uint32_t mode,
                                                  uint32_t *res_sector, uint64_t *res_cluster,
                                                  uint64_t *res_data_buf_base)
{
    // kdebug("find empty_dentry");
    struct fat32_inode_info_t *finode = (struct fat32_inode_info_t *)parent_inode->private_inode_info;
    fat32_sb_info_t *fsbi = (fat32_sb_info_t *)parent_inode->sb->private_sb_info;

    uint8_t *buf = kzalloc(fsbi->bytes_per_clus, 0);

    struct block_device *blk = parent_inode->sb->blk_device;

    // 计算父目录项的起始簇号
    uint32_t cluster = finode->first_clus;

    struct fat32_Directory_t *tmp_dEntry = NULL;
    // 指向最终的有用的dentry的指针
    struct fat32_Directory_t *result_dEntry = NULL;

    while (true)
    {
        // 计算父目录项的起始LBA扇区号
        uint64_t sector = fsbi->first_data_sector + (cluster - 2) * fsbi->sec_per_clus;

        // 读取父目录项的起始簇数据
        blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_READ_DMA_EXT, sector, fsbi->sec_per_clus, (uint64_t)buf);
        tmp_dEntry = (struct fat32_Directory_t *)buf;
        // 计数连续的空目录项
        uint32_t count_continuity = 0;

        // 查找连续num个空闲目录项
        for (int i = 0; (i < fsbi->bytes_per_clus) && count_continuity < num; i += 32, ++tmp_dEntry)
        {
            if (!(tmp_dEntry->DIR_Name[0] == 0xe5 || tmp_dEntry->DIR_Name[0] == 0x00 ||
                  tmp_dEntry->DIR_Name[0] == 0x05))
            {
                count_continuity = 0;
                continue;
            }

            if (count_continuity == 0)
                result_dEntry = tmp_dEntry;
            ++count_continuity;
        }

        // 成功查找到符合要求的目录项
        if (count_continuity == num)
        {
            result_dEntry += (num - 1);
            *res_sector = sector;
            *res_data_buf_base = (uint64_t)buf;
            *res_cluster = cluster;

            return result_dEntry;
        }

        // 当前簇没有发现符合条件的空闲目录项，寻找下一个簇
        uint64_t old_cluster = cluster;
        cluster = fat32_read_FAT_entry(blk, fsbi, cluster);
        if (cluster >= 0x0ffffff7) // 寻找完父目录的所有簇，都没有找到符合要求的空目录项
        {

            // 新增一个簇

            if (fat32_alloc_clusters(parent_inode, &cluster, 1) != 0)
            {
                kerror("Cannot allocate a new cluster!");
                while (1)
                    pause();
            }

            // 将这个新的簇清空
            sector = fsbi->first_data_sector + (cluster - 2) * fsbi->sec_per_clus;
            void *tmp_buf = kzalloc(fsbi->bytes_per_clus, 0);
            blk->bd_disk->fops->transfer(blk->bd_disk, AHCI_CMD_WRITE_DMA_EXT, sector, fsbi->sec_per_clus,
                                         (uint64_t)tmp_buf);
            kfree(tmp_buf);
        }
    }
}

/**
 * @brief 检查文件名是否合法
 *
 * @param name 文件名
 * @param namelen 文件名长度
 * @param reserved 保留字段
 * @return int 合法：0， 其他：错误码
 */
int fat32_check_name_available(const char *name, int namelen, int8_t reserved)
{
    if (namelen > 255 || namelen <= 0)
        return -ENAMETOOLONG;
    // 首个字符不能是空格或者'.'
    if (name[0] == 0x20 || name[0] == '.')
        return -EINVAL;

    return 0;
}

/**
 * @brief 检查字符在短目录项中是否合法
 *
 * @param c 给定字符
 * @param index 字符在文件名中处于第几位
 * @return true 合法
 * @return false 不合法
 */
bool fat32_check_char_available_in_short_name(const char c, int index)
{
    // todo: 严格按照fat规范完善合法性检查功能
    if (index == 0)
    {
        if (c < 0x20)
        {
            if (c != 0x05)
                return false;
            return true;
        }
    }

    for (int i = 0; i < sizeof(unavailable_character_in_short_name) / sizeof(char); ++i)
    {
        if (c == unavailable_character_in_short_name[i])
            return false;
    }
    return true;
}

/**
 * @brief 填充短目录项的函数
 *
 * @param dEntry 目标dentry
 * @param target 目标dentry对应的短目录项
 * @param cluster 短目录项对应的文件/文件夹起始簇
 */
void fat32_fill_shortname(struct vfs_dir_entry_t *dEntry, struct fat32_Directory_t *target, uint32_t cluster)
{
    memset(target, 0, sizeof(struct fat32_Directory_t));
    {
        int tmp_index = 0;
        // kdebug("dEntry->name_length=%d", dEntry->name_length);
        for (tmp_index = 0; tmp_index < min(8, dEntry->name_length); ++tmp_index)
        {
            if (dEntry->name[tmp_index] == '.')
                break;
            if (fat32_check_char_available_in_short_name(dEntry->name[tmp_index], tmp_index))
                target->DIR_Name[tmp_index] = dEntry->name[tmp_index];
            else
                target->DIR_Name[tmp_index] = 0x20;
        }

        // 不满的部分使用0x20填充
        while (tmp_index < 8)
        {
            // kdebug("tmp index = %d", tmp_index);
            target->DIR_Name[tmp_index] = 0x20;
            ++tmp_index;
        }
        if (dEntry->dir_inode->attribute & VFS_IF_DIR)
        {
            while (tmp_index < 11)
            {
                // kdebug("tmp index = %d", tmp_index);
                target->DIR_Name[tmp_index] = 0x20;
                ++tmp_index;
            }
        }
        else
        {
            for (int j = 8; j < 11; ++j)
            {
                target->DIR_Name[j] = 'a';
            }
        }
    }

    struct vfs_index_node_t *inode = dEntry->dir_inode;
    target->DIR_Attr = 0;
    if (inode->attribute & VFS_IF_DIR)
        target->DIR_Attr |= ATTR_DIRECTORY;

    target->DIR_FileSize = dEntry->dir_inode->file_size;
    target->DIR_FstClusHI = (uint16_t)((cluster >> 16) & 0x0fff);
    target->DIR_FstClusLO = (uint16_t)(cluster & 0xffff);

    // todo: 填写短目录项中的时间信息
}

/**
 * @brief 填充长目录项的函数
 *
 * @param dEntry 目标dentry
 * @param target 起始长目录项
 * @param checksum 短目录项的校验和
 * @param cnt_longname 总的长目录项的个数
 */
void fat32_fill_longname(struct vfs_dir_entry_t *dEntry, struct fat32_LongDirectory_t *target, uint8_t checksum,
                         uint32_t cnt_longname)
{
    uint32_t current_name_index = 0;
    struct fat32_LongDirectory_t *Ldentry = (struct fat32_LongDirectory_t *)(target + 1);
    // kdebug("filling long name, name=%s, namelen=%d", dEntry->name, dEntry->name_length);
    int name_length = dEntry->name_length + 1;
    for (int i = 1; i <= cnt_longname; ++i)
    {
        --Ldentry;

        Ldentry->LDIR_Ord = i;

        for (int j = 0; j < 5; ++j, ++current_name_index)
        {
            if (current_name_index < name_length)
                Ldentry->LDIR_Name1[j] = dEntry->name[current_name_index];
            else
                Ldentry->LDIR_Name1[j] = 0xffff;
        }
        for (int j = 0; j < 6; ++j, ++current_name_index)
        {
            if (current_name_index < name_length)
                Ldentry->LDIR_Name2[j] = dEntry->name[current_name_index];
            else
                Ldentry->LDIR_Name2[j] = 0xffff;
        }
        for (int j = 0; j < 2; ++j, ++current_name_index)
        {
            if (current_name_index < name_length)
                Ldentry->LDIR_Name3[j] = dEntry->name[current_name_index];
            else
                Ldentry->LDIR_Name3[j] = 0xffff;
        }
        Ldentry->LDIR_Attr = ATTR_LONG_NAME;
        Ldentry->LDIR_FstClusLO = 0;
        Ldentry->LDIR_Type = 0;
        Ldentry->LDIR_Chksum = checksum;
    }

    // 最后一个长目录项的ord要|=0x40
    Ldentry->LDIR_Ord |= 0x40;
}

/**
 * @brief 删除目录项
 *
 * @param dir 父目录的inode
 * @param sinfo 待删除的dentry的插槽信息
 * @return int 错误码
 */
int fat32_remove_entries(struct vfs_index_node_t *dir, struct fat32_slot_info *sinfo)
{
    int retval = 0;
    struct vfs_superblock_t *sb = dir->sb;
    struct fat32_Directory_t *de = sinfo->de;
    fat32_sb_info_t *fsbi = (fat32_sb_info_t *)sb->private_sb_info;
    int cnt_dentries = sinfo->num_slots;

    // 获取文件数据区的起始簇号
    int data_cluster = ((((uint32_t)de->DIR_FstClusHI) << 16) | ((uint32_t)de->DIR_FstClusLO)) & 0x0fffffff;
    // kdebug("data_cluster=%d, cnt_dentries=%d, offset=%d", data_cluster, cnt_dentries, sinfo->slot_off);
    // kdebug("fsbi->first_data_sector=%d, sec per clus=%d, i_pos=%d", fsbi->first_data_sector, fsbi->sec_per_clus,
    //        sinfo->i_pos);
    // === 第一阶段，先删除短目录项
    while (cnt_dentries > 0)
    {
        de->DIR_Name[0] = FAT32_DELETED_FLAG;
        --cnt_dentries;
        --de;
    }

    // === 第二阶段：将对目录项的更改写入磁盘

    sb->blk_device->bd_disk->fops->transfer(sb->blk_device->bd_disk, AHCI_CMD_WRITE_DMA_EXT, sinfo->i_pos,
                                            fsbi->sec_per_clus, (uint64_t)sinfo->buffer);

    // === 第三阶段：清除文件的数据区
    uint32_t next_clus;
    int js = 0;
    // kdebug("data_cluster=%#018lx", data_cluster);
    while (data_cluster < 0x0ffffff8 && data_cluster >= 2)
    {
        // 读取下一个表项
        next_clus = fat32_read_FAT_entry(sb->blk_device, fsbi, data_cluster);
        // kdebug("data_cluster=%#018lx, next_clus=%#018lx", data_cluster, next_clus);
        // 清除当前表项
        retval = fat32_write_FAT_entry(sb->blk_device, fsbi, data_cluster, 0);
        if (unlikely(retval != 0))
        {
            kerror("fat32_remove_entries: Failed to mark fat entry as unused for cluster:%d", data_cluster);
            goto out;
        }
        ++js;
        data_cluster = next_clus;
    }
out:;
    // kdebug("Successfully remove %d clusters.", js);
    return retval;
}