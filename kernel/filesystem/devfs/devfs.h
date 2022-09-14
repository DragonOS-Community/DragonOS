#pragma once

#include "devfs-types.h"

/**
 * @brief 初始化devfs
 * 
 */
void devfs_init();

/**
 * @brief 在devfs中注册设备
 *
 * @param device_type 设备主类型
 * @param sub_type 设备子类型
 * @param file_ops 设备的文件操作接口
 * @return int 错误码
 */
int devfs_register_device(uint16_t device_type, uint16_t sub_type, struct vfs_file_operations_t *file_ops);