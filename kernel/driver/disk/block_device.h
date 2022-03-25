#pragma once

#include "../../common/glib.h"
#include "stdint.h"

struct block_device_operation
{
    long (*open)();
    long (*close)();
    long (*ioctl)(long cmd, long arg);
    long (*transfer)(long cmd, ul LBA_start, ul count, uchar* buffer);
};

/**
 * @brief 块设备请求队列内的packet
 * 
 */
struct block_device_request_packet
{
    uchar cmd;
    uint32_t LBA_start;
    uint32_t count;
    uchar *buffer;

    void (*end_handler)(ul num, ul arg);

    struct List list;
};

/**
 * @brief 块设备的请求队列
 * 
 */
struct block_device_request_queue
{
    struct List queue_list;
    struct block_device_request_packet * entry;
    ul request_count;
};