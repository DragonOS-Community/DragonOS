/**
 * @file fat32.h
 * @author fslongjin (longjin@RinGoTek.cn)
 * @brief fat32文件系统
 * @version 0.1
 * @date 2022-04-19
 *
 * @copyright Copyright (c) 2022
 *
 */

#pragma once

#include <filesystem/MBR.h>
#include <filesystem/VFS/VFS.h>

#define FAT32_MAX_PARTITION_NUM 128 // 系统支持的最大的fat32分区数量

#define FAT32_DELETED_FLAG 0xe5 // 如果短目录项的name[0]为这个值，那么意味着这个短目录项是空闲的

/**
 * @brief fat32文件系统引导扇区结构体
 *
 */
struct fat32_BootSector_t
{
    uint8_t BS_jmpBoot[3];    // 跳转指令
    uint8_t BS_OEMName[8];    // 生产厂商名
    uint16_t BPB_BytesPerSec; // 每扇区字节数
    uint8_t BPB_SecPerClus;   // 每簇扇区数
    uint16_t BPB_RsvdSecCnt;  // 保留扇区数
    uint8_t BPB_NumFATs;      // FAT表数量
    uint16_t BPB_RootEntCnt;  // 根目录文件数最大值
    uint16_t BPB_TotSec16;    // 16位扇区总数
    uint8_t BPB_Media;        // 介质描述符
    uint16_t BPB_FATSz16;     // FAT12/16每FAT扇区数
    uint16_t BPB_SecPerTrk;   // 每磁道扇区数
    uint16_t BPB_NumHeads;    // 磁头数
    uint32_t BPB_HiddSec;     // 隐藏扇区数
    uint32_t BPB_TotSec32;    // 32位扇区总数

    uint32_t BPB_FATSz32;   // FAT32每FAT扇区数
    uint16_t BPB_ExtFlags;  // 扩展标志
    uint16_t BPB_FSVer;     // 文件系统版本号
    uint32_t BPB_RootClus;  // 根目录起始簇号
    uint16_t BPB_FSInfo;    // FS info结构体的扇区号
    uint16_t BPB_BkBootSec; // 引导扇区的备份扇区号
    uint8_t BPB_Reserved0[12];

    uint8_t BS_DrvNum; // int0x13的驱动器号
    uint8_t BS_Reserved1;
    uint8_t BS_BootSig;       // 扩展引导标记
    uint32_t BS_VolID;        // 卷序列号
    uint8_t BS_VolLab[11];    // 卷标
    uint8_t BS_FilSysType[8]; // 文件系统类型

    uint8_t BootCode[420]; // 引导代码、数据

    uint16_t BS_TrailSig; // 结束标志0xAA55
} __attribute__((packed));

/**
 * @brief fat32文件系统的FSInfo扇区结构体
 *
 */
struct fat32_FSInfo_t
{
    uint32_t FSI_LeadSig;       // FS info扇区标志符 数值为0x41615252
    uint8_t FSI_Reserved1[480]; // 保留使用，全部置为0
    uint32_t FSI_StrucSig;      // 另一个标志符，数值为0x61417272
    uint32_t FSI_Free_Count;    // 上一次记录的空闲簇数量，这是一个参考值
    uint32_t FSI_Nxt_Free;      // 空闲簇的起始搜索位置，这是为驱动程序提供的参考值
    uint8_t FSI_Reserved2[12];  // 保留使用，全部置为0
    uint32_t FSI_TrailSig;      // 结束标志，数值为0xaa550000
} __attribute__((packed));

#define ATTR_READ_ONLY (1 << 0)
#define ATTR_HIDDEN (1 << 1)
#define ATTR_SYSTEM (1 << 2)
#define ATTR_VOLUME_ID (1 << 3)
#define ATTR_DIRECTORY (1 << 4)
#define ATTR_ARCHIVE (1 << 5)
#define ATTR_LONG_NAME (ATTR_READ_ONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_VOLUME_ID)

/**
 * @brief fat32文件系统短目录项,大小为32bytes
 *
 */
struct fat32_Directory_t
{
    unsigned char DIR_Name[11];
    unsigned char DIR_Attr;         // 目录项属性
    unsigned char DIR_NTRes;        // EXT|BASE => 8(BASE).3(EXT)
                                    // BASE:LowerCase(8),UpperCase(0)
                                    // EXT:LowerCase(16),UpperCase(0)
    unsigned char DIR_CrtTimeTenth; // 文件创建的毫秒级时间戳
    unsigned short DIR_CrtTime;     // 文件创建时间
    unsigned short DIR_CrtDate;     // 文件创建日期
    unsigned short DIR_LastAccDate; // 文件的最后访问日期
    unsigned short DIR_FstClusHI;   // 起始簇号（高16bit）
    unsigned short DIR_WrtTime;     // 最后写入时间
    unsigned short DIR_WrtDate;     // 最后写入日期
    unsigned short DIR_FstClusLO;   // 起始簇号（低16bit）
    unsigned int DIR_FileSize;      // 文件大小
} __attribute__((packed));

#define LOWERCASE_BASE (8)
#define LOWERCASE_EXT (16)

/**
 * @brief fat32文件系统长目录项,大小为32bytes
 *
 */
struct fat32_LongDirectory_t
{
    unsigned char LDIR_Ord;        // 长目录项的序号
    unsigned short LDIR_Name1[5];  // 长文件名的第1-5个字符，每个字符占2bytes
    unsigned char LDIR_Attr;       // 目录项属性必须为ATTR_LONG_NAME
    unsigned char LDIR_Type;       // 如果为0，则说明这是长目录项的子项
    unsigned char LDIR_Chksum;     // 短文件名的校验和
    unsigned short LDIR_Name2[6];  // 长文件名的第6-11个字符，每个字符占2bytes
    unsigned short LDIR_FstClusLO; // 必须为0
    unsigned short LDIR_Name3[2];  // 长文件名的12-13个字符，每个字符占2bytes
} __attribute__((packed));

/**
 * @brief fat32文件系统的超级块信息结构体
 *
 */
struct fat32_partition_info_t
{
    uint16_t partition_id; // 全局fat32分区id
    // todo: 增加mutex，使得对fat32文件系统的访问是互斥的

    struct fat32_BootSector_t bootsector;
    struct fat32_FSInfo_t fsinfo;
    uint64_t fsinfo_sector_addr_infat;
    uint64_t bootsector_bak_sector_addr_infat;

    uint64_t starting_sector;
    uint64_t sector_count;

    uint64_t sec_per_clus;   // 每簇扇区数
    uint64_t bytes_per_sec;  // 每扇区字节数
    uint64_t bytes_per_clus; // 每簇字节数

    uint64_t first_data_sector; // 数据区起始扇区号
    uint64_t FAT1_base_sector;  // FAT1表的起始簇号
    uint64_t FAT2_base_sector;  // FAT2表的起始簇号
    uint64_t sec_per_FAT;       // 每FAT表扇区数
    uint64_t NumFATs;           // FAT表数
};

typedef struct fat32_partition_info_t fat32_sb_info_t;

struct fat32_inode_info_t
{
    uint32_t first_clus;                  // 文件的起始簇号
    uint64_t dEntry_location_clus;        // fat entry的起始簇号 dEntry struct in cluster (0 is root, 1 is invalid)
    uint64_t dEntry_location_clus_offset; // fat entry在起始簇中的偏移量(是第几个entry) dEntry struct offset in cluster

    uint16_t create_date;
    uint16_t create_time;
    uint16_t write_time;
    uint16_t write_date;
};

typedef struct fat32_inode_info_t fat32_inode_info_t;

/**
 * @brief FAT32目录项插槽信息
 * 一个插槽指的是 一个长目录项/短目录项
 */
struct fat32_slot_info
{
    off_t i_pos;    // on-disk position of directory entry(扇区号)
    off_t slot_off; // offset for slot or (de) start
    int num_slots;  // number of slots +1(de) in filename
    struct fat32_Directory_t * de;
    
    // todo: 加入block io层后，在这里引入buffer_head
    void *buffer;   // 记得释放这个buffer！！！
};

/**
 * @brief 注册指定磁盘上的指定分区的fat32文件系统
 *
 * @param blk_dev 块设备结构体
 * @param part_num 磁盘分区编号
 *
 * @return struct vfs_super_block_t * 文件系统的超级块
 */
struct vfs_superblock_t *fat32_register_partition(struct block_device *blk_dev, uint8_t part_num);

/**
 * @brief 创建fat32文件系统的超级块
 *
 * @param blk 块设备结构体
 * @return struct vfs_superblock_t* 创建好的超级块
 */
struct vfs_superblock_t *fat32_read_superblock(struct block_device *blk);

/**
 * @brief 创建新的文件
 * @param parent_inode 父目录的inode结构体
 * @param dest_dEntry 新文件的dentry
 * @param mode 创建模式
 */
long fat32_create(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dest_dEntry, int mode);

void fat32_init();

/**
 * @brief 读取文件夹(在指定目录中找出有效目录项)
 *
 * @param file_ptr 文件结构体指针
 * @param dirent 返回的dirent
 * @param filler 填充dirent的函数
 * @return int64_t
 */
int64_t fat32_readdir(struct vfs_file_t *file_ptr, void *dirent, vfs_filldir_t filler);