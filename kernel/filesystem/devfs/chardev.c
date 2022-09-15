#include "chardev.h"
#include "internal.h"
#include <filesystem/VFS/VFS.h>

#include <common/mutex.h>
#include <common/stdlib.h>
#include <common/string.h>
#include <common/printk.h>

static struct vfs_dir_entry_t *chardev_folder_dentry = NULL;
extern struct vfs_dir_entry_t *devfs_root_dentry;

/**
 * @brief 字符设备名称前缀
 *
 */
static char chardev_name_prefix[CHAR_DEV_STYPE_END + 1][32] = {
    [CHAR_DEV_STYPE_START] = "",
    [CHAR_DEV_STYPE_PS2_KEYBOARD] = "ps2.kb",
    [CHAR_DEV_STYPE_PS2_MOUSE] = "ps2.mse",
    [CHAR_DEV_STYPE_USB_MOUSE] = "usb.mse",
    [CHAR_DEV_STYPE_USB_KEYBOARD] = "usb.kb",
    [CHAR_DEV_STYPE_BLUETOOTH_MOUSE] = "bt.mse",
    [CHAR_DEV_STYPE_BLUETOOTH_KEYBOARD] = "bt.kb",
    [CHAR_DEV_STYPE_TTY] = "vdev.tty",
    [CHAR_DEV_STYPE_END] = "",
};
/**
 * @brief 为不同类型的字符设备分配的管理信息结构体
 *
 */
static struct chardev_manage_info_t
{
    mutex_t lock; // 操作互斥锁
    int count;
} chardev_manage_info[CHAR_DEV_STYPE_END + 1];

/**
 * @brief 在devfs中注册字符设备（该函数只应被devfs调用）
 *
 * @param private_info inode私有信息
 * @param target_dentry 返回的dentry的指针
 * @return int 错误码
 */
int __devfs_chardev_register(struct devfs_private_inode_info_t *private_info, struct vfs_dir_entry_t **target_dentry)
{
    // 检测subtype是否合法
    if (private_info->sub_type <= CHAR_DEV_STYPE_START || private_info->sub_type >= CHAR_DEV_STYPE_END)
        return -EINVAL;
    mutex_lock(&chardev_manage_info[private_info->sub_type].lock);

    // 拷贝名称
    char devname[64] = {0};
    strcpy(devname, chardev_name_prefix[private_info->sub_type]);
    char *ptr = devname + strlen(chardev_name_prefix[private_info->sub_type]);
    sprintk(ptr, "%d", chardev_manage_info[private_info->sub_type].count);
    int namelen = strlen(devname);

    struct vfs_dir_entry_t *dentry = vfs_alloc_dentry(namelen + 1);
    __devfs_fill_dentry(dentry, devname);
    __devfs_fill_inode(dentry, vfs_alloc_inode(), VFS_IF_DEVICE, private_info);

    // 将dentry挂载到char文件夹下
    __devfs_dentry_bind_parent(chardev_folder_dentry, dentry);

    ++chardev_manage_info[private_info->sub_type].count;
    mutex_unlock(&chardev_manage_info[private_info->sub_type].lock);
    *target_dentry = dentry;
    return 0;
}

/**
 * @brief 初始化chardev管理机制
 *
 */
void __devfs_chardev_init()
{
    // 初始化管理信息结构体
    for (int i = CHAR_DEV_STYPE_START + 1; i < CHAR_DEV_STYPE_END; ++i)
    {
        mutex_init(&chardev_manage_info[i].lock);
        chardev_manage_info[i].count = 0;
    }

    vfs_mkdir("/dev/char", 0, false);
    // 获取char dev的dentry
    chardev_folder_dentry = __devfs_find_dir(devfs_root_dentry, "char");
}