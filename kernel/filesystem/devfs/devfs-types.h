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
    DEV_TYPE_USB,
    DEV_TYPE_BLOCK,
    DEV_TYPE_NET,
    DEV_TYPE_BUS,

};

/**
 * @brief 字符设备的sub_type字段值
 *
 */
enum
{
    CHAR_DEV_STYPE_START = 0,
    CHAR_DEV_STYPE_PS2_KEYBOARD = 1,
    CHAR_DEV_STYPE_USB_KEYBOARD,
    CHAR_DEV_STYPE_PS2_MOUSE,
    CHAR_DEV_STYPE_USB_MOUSE,
    CHAR_DEV_STYPE_BLUETOOTH_MOUSE,
    CHAR_DEV_STYPE_BLUETOOTH_KEYBOARD,
    CHAR_DEV_STYPE_TTY,
    CHAR_DEV_STYPE_END, // 结束标志
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
    uint64_t uuid;
    struct vfs_index_node_t * inode;    // 当前私有信息所绑定的inode
};
