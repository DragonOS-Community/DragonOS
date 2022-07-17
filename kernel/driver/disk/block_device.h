#pragma once

#include <common/glib.h>
#include "stdint.h"
#include <process/semaphore.h>

#define BLK_TYPE_AHCI 0
struct block_device_operation
{
    long (*open)();
    long (*close)();
    long (*ioctl)(long cmd, long arg);
    long (*transfer)(long cmd, ul LBA_start, ul count, uint64_t buffer, uint8_t arg0, uint8_t arg1);
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

    uint8_t device_type;    // 0: ahci
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
    struct block_device_request_packet * in_service;    // 正在请求的结点
    ul request_count;
};