#pragma once

#include "fat32.h"
#include <filesystem/VFS/VFS.h>
#include <stdbool.h>

/**
 * @brief 请求分配指定数量的簇
 *
 * @param inode 要分配簇的inode
 * @param clusters 返回的被分配的簇的簇号结构体
 * @param num_clusters 要分配的簇的数量
 * @return int 错误码
 */
int fat32_alloc_clusters(struct vfs_index_node_t *inode, uint32_t *clusters, int32_t num_clusters);

/**
 * @brief 释放从属于inode的，从cluster开始的所有簇
 *
 * @param inode 指定的文件的inode
 * @param cluster 指定簇
 * @return int 错误码
 */
int fat32_free_clusters(struct vfs_index_node_t *inode, int32_t cluster);

/**
 * @brief 读取指定簇的FAT表项
 *
 * @param blk 块设备结构体
 * @param fsbi fat32超级块私有信息结构体
 * @param cluster 指定簇
 * @return uint32_t 下一个簇的簇号
 */
uint32_t fat32_read_FAT_entry(struct block_device * blk, fat32_sb_info_t *fsbi, uint32_t cluster);

/**
 * @brief 写入指定簇的FAT表项
 *
 * @param blk 块设备结构体
 * @param fsbi fat32超级块私有信息结构体
 * @param cluster 指定簇
 * @param value 要写入该fat表项的值
 * @return uint32_t errcode
 */
int fat32_write_FAT_entry(struct block_device * blk, fat32_sb_info_t *fsbi, uint32_t cluster, uint32_t value);

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
struct fat32_Directory_t *fat32_find_empty_dentry(struct vfs_index_node_t *parent_inode, uint32_t num, uint32_t mode, uint32_t *res_sector, uint64_t *res_cluster, uint64_t *res_data_buf_base);

/**
 * @brief 检查文件名是否合法
 *
 * @param name 文件名
 * @param namelen 文件名长度
 * @param reserved 保留字段
 * @return int 合法：0， 其他：错误码
 */
int fat32_check_name_available(const char *name, int namelen, int8_t reserved);

/**
 * @brief 检查字符在短目录项中是否合法
 *
 * @param c 给定字符
 * @param index 字符在文件名中处于第几位
 * @return true 合法
 * @return false 不合法
 */
bool fat32_check_char_available_in_short_name(const char c, int index);

/**
 * @brief 填充短目录项的函数
 * 
 * @param dEntry 目标dentry
 * @param target 目标dentry对应的短目录项
 * @param cluster 短目录项对应的文件/文件夹起始簇
 */
void fat32_fill_shortname(struct vfs_dir_entry_t *dEntry, struct fat32_Directory_t *target, uint32_t cluster);

/**
 * @brief 填充长目录项的函数
 * 
 * @param dEntry 目标dentry
 * @param target 起始长目录项
 * @param checksum 短目录项的校验和
 * @param cnt_longname 总的长目录项的个数
 */
void fat32_fill_longname(struct vfs_dir_entry_t *dEntry, struct fat32_LongDirectory_t *target, uint8_t checksum, uint32_t cnt_longname);

int fat32_remove_entries(struct vfs_index_node_t *dir, struct fat32_slot_info *sinfo);