#pragma once
#include "blk_types.h"

/**
 * @brief 将磁盘注册到块设备框架中
 * 
 * @param gendisk 磁盘结构体
 * @return int 错误码
 */
int blk_register_gendisk(struct blk_gendisk * gendisk);