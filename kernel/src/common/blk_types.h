#pragma once

#include <DragonOS/stdint.h>
#include <common/glib.h>

#define BLK_TYPE_AHCI 0

#define DISK_NAME_LEN 32 // 磁盘名称的最大长度



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
};

