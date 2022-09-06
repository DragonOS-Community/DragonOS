#pragma once
#include <filesystem/VFS/VFS.h>

/**
 * @brief devfs_private_file_info_t的type字段值
 * 
 */
enum
{
    DEV_TYPE_UNDEF = 0,
    DEV_TYPE_CHAR = 1,
};

/**
 * @brief 字符设备的sub_type字段值
 * 
 */
enum
{
    CHAR_DEV_STYPE_PS2 = 1,
    CHAR_DEV_STYPE_USB,
    CHAR_DEV_STYPE_BLUETOOTH,
};

/**
 * @brief 设备文件私有信息结构体
 *
 */
struct devfs_private_inode_info_t
{
    uint16_t type;     // 设备主类型
    uint16_t sub_type; // 设备子类型
    struct vfs_file_operations_t *f_ops;
};
