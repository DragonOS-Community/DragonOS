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
 * @param ret_private_inode_info_ptr 返回的指向inode私有信息结构体的指针
 * @return int 错误码
 */
int devfs_register_device(uint16_t device_type, uint16_t sub_type, struct vfs_file_operations_t *file_ops, struct devfs_private_inode_info_t **ret_private_inode_info_ptr);

/**
 * @brief 卸载设备
 * 
 * @param private_inode_info 待卸载的设备的inode私有信息
 * @param put_private_info 设备被卸载后，执行的函数
 * @return int 错误码
 */
int devfs_unregister_device(struct devfs_private_inode_info_t * private_inode_info);