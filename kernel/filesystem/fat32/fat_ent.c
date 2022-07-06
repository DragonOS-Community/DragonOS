#include "fat_ent.h"
#include <driver/disk/ahci/ahci.h>
#include <common/errno.h>
#include <mm/slab.h>

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

    uint64_t sec_per_fat = fsbi->sec_per_FAT;

    // todo: 对alloc的过程加锁

    // 申请1扇区的缓冲区
    uint32_t *buf = (uint32_t *)kmalloc(fsbi->bytes_per_sec, 0);
    int ent_per_sec = (fsbi->bytes_per_sec >> 2);
    int clus_idx = 0;
    for (int i = 0; i < sec_per_fat; ++i)
    {
        if (clus_idx >= num_clusters)
            goto done;
        memset(buf, 0, fsbi->bytes_per_sec);

        ahci_operation.transfer(AHCI_CMD_READ_DMA_EXT, fsbi->FAT1_base_sector + i, 1, (uint64_t)buf, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);
        // 依次检查簇是否空闲
        for (int j = 0; j < ent_per_sec; ++j)
        {
            if (clus_idx >= num_clusters)
                goto done;
            // 找到空闲簇
            if ((buf[j] & 0x0fffffff) == 0)
            {
                kdebug("clus[%d] = %d", clus_idx, i * ent_per_sec + j);
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
            // todo: 跳转到文件当前的最后一个簇
            idx = 0;
            int tmp_clus = finode->first_clus;
            cluster = tmp_clus;
            while (true)
            {
                tmp_clus = fat32_read_FAT_entry(fsbi, cluster);
                if (tmp_clus <= 0x0ffffff7)
                    cluster = tmp_clus;
                else
                    break;
            }
        }

        // 写入fat表
        for (int i = idx; i < num_clusters; ++i)
        {
            kdebug("write cluster i=%d : cluster=%d, value= %d", i, cluster, clusters[i]);
            fat32_write_FAT_entry(fsbi, cluster, clusters[i]);
            cluster = clusters[i];
        }
        fat32_write_FAT_entry(fsbi, cluster, 0x0ffffff8);

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
    uint32_t *buf = kmalloc(fsbi->bytes_per_sec, 0);
    memset(buf, 0, fsbi->bytes_per_sec);

    ahci_operation.transfer(AHCI_CMD_READ_DMA_EXT, fsbi->FAT1_base_sector + (cluster / fat_ent_per_sec), 1,
                            (uint64_t)buf, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);

    buf[cluster & (fat_ent_per_sec - 1)] = (buf[cluster & (fat_ent_per_sec - 1)] & 0xf0000000) | (value & 0x0fffffff);
    // 向FAT1和FAT2写入数据
    ahci_operation.transfer(AHCI_CMD_WRITE_DMA_EXT, fsbi->FAT1_base_sector + (cluster / fat_ent_per_sec), 1,
                            (uint64_t)buf, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);
    ahci_operation.transfer(AHCI_CMD_WRITE_DMA_EXT, fsbi->FAT2_base_sector + (cluster / fat_ent_per_sec), 1,
                            (uint64_t)buf, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);
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
 * @return struct fat32_Directory_t* 符合要求的entry的指针（指向地址高处的空目录项，也就是说，有连续num个≤这个指针的空目录项）
 */
struct fat32_Directory_t *fat32_find_empty_dentry(struct vfs_index_node_t *parent_inode, uint32_t num, uint32_t mode, uint32_t *res_sector, uint64_t *res_cluster, uint64_t *res_data_buf_base)
{
    kdebug("find empty_dentry");
    struct fat32_inode_info_t *finode = (struct fat32_inode_info_t *)parent_inode->private_inode_info;
    fat32_sb_info_t *fsbi = (fat32_sb_info_t *)parent_inode->sb->private_sb_info;

    uint8_t *buf = kmalloc(fsbi->bytes_per_clus, 0);
    memset(buf, 0, fsbi->bytes_per_clus);

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
        ahci_operation.transfer(AHCI_CMD_READ_DMA_EXT, sector, fsbi->sec_per_clus, (uint64_t)buf, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);
        tmp_dEntry = (struct fat32_Directory_t *)buf;
        // 计数连续的空目录项
        uint32_t count_continuity = 0;

        // 查找连续num个空闲目录项
        for (int i = 0; (i < fsbi->bytes_per_clus) && count_continuity < num; i += 32, ++tmp_dEntry)
        {
            if (!(tmp_dEntry->DIR_Name[0] == 0xe5 || tmp_dEntry->DIR_Name[0] == 0x00))
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
        cluster = fat32_read_FAT_entry(fsbi, cluster);
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
            void *tmp_buf = kmalloc(fsbi->bytes_per_clus, 0);
            memset(tmp_buf, 0, fsbi->bytes_per_clus);
            ahci_operation.transfer(AHCI_CMD_WRITE_DMA_EXT, sector, fsbi->sec_per_clus, (uint64_t)tmp_buf, fsbi->ahci_ctrl_num, fsbi->ahci_port_num);
            kfree(tmp_buf);
        }
    }
}