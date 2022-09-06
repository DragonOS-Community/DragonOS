#pragma once

#include <common/glib.h>
#include "stdint.h"
#include <common/semaphore.h>
#include <common/mutex.h>

#define BLK_TYPE_AHCI 0

#define DISK_NAME_LEN 32 // 磁盘名称的最大长度

struct blk_gendisk;

struct block_device_operation
{
    long (*open)();
    long (*close)();
    long (*ioctl)(long cmd, long arg);
    
    /**
     * @brief 块设备驱动程序的传输函数
     *
     * @param gd 磁盘设备结构体
     * @param cmd 控制命令
     * @param base_addr 48位LBA地址
     * @param count total sectors to read
     * @param buf 缓冲区线性地址
     * @return long
     */
    long (*transfer)(struct blk_gendisk *gd, long cmd, uint64_t base_addr, uint64_t count, uint64_t buf);
};

/**
 * @brief 块设备请求队列内的packet
 *
 */
struct block_device_request_packet
{
    uchar cmd;
    uint64_t LBA_start;
    uint32_t count;
    uint64_t buffer_vaddr;

    uint8_t device_type; // 0: ahci
    void (*end_handler)(ul num, ul arg);

    wait_queue_node_t wait_queue;
};

/**
 * @brief 块设备的请求队列
 *
 */
struct block_device_request_queue
{
    wait_queue_node_t wait_queue_list;
    struct block_device_request_packet *in_service; // 正在请求的结点
    ul request_count;
};

/**
 * @brief 块设备结构体（对应磁盘的一个分区）
 *
 */
struct block_device
{
    sector_t bd_start_sector;                    // 该分区的起始扇区
    uint64_t bd_start_LBA;                       // 起始LBA号
    sector_t bd_sectors_num;                     // 该分区的扇区数
    struct vfs_superblock_t *bd_superblock;      // 执行超级块的指针
    struct blk_gendisk *bd_disk;                 // 当前分区所属的磁盘
    struct block_device_request_queue *bd_queue; // 请求队列
    uint16_t bd_partno;                          // 在磁盘上的分区号
};

// 定义blk_gendisk中的标志位
#define BLK_GF_AHCI (1 << 0)

/**
 * @brief 磁盘设备结构体
 *
 */
struct blk_gendisk
{
    char disk_name[DISK_NAME_LEN]; // 磁盘驱动器名称
    uint16_t part_cnt;             // 磁盘分区计数
    uint16_t flags;
    struct block_device *partition;                   // 磁盘分区数组
    const struct block_device_operation *fops;        // 磁盘操作
    struct block_device_request_queue *request_queue; // 磁盘请求队列
    void *private_data;

    mutex_t open_mutex; // open()/close()操作的互斥锁
};