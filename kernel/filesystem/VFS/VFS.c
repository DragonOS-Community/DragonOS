#include "VFS.h"
#include <common/kprint.h>
#include <mm/slab.h>

// 为filesystem_type_t结构体实例化一个链表头
static struct vfs_filesystem_type_t vfs_fs = {"filesystem", 0};

/**
 * @brief 挂载文件系统
 *
 * @param name 文件系统名
 * @param DPTE 分区表entry
 * @param DPT_type 分区表类型
 * @param buf 文件系统的引导扇区
 * @return struct vfs_superblock_t*
 */
struct vfs_superblock_t *vfs_mount_fs(char *name, void *DPTE, uint8_t DPT_type, void *buf, int8_t ahci_ctrl_num, int8_t ahci_port_num, int8_t part_num)
{

    struct vfs_filesystem_type_t *p = NULL;
    for (p = &vfs_fs; p; p = p->next)
    {
        if (!strcmp(p->name, name)) // 存在符合的文件系统
        {
            return p->read_superblock(DPTE, DPT_type, buf, ahci_ctrl_num, ahci_port_num, part_num);
        }
    }
    kdebug("unsupported fs: %s", name);
    return NULL;
}

/**
 * @brief 在VFS中注册文件系统
 *
 * @param fs 文件系统类型结构体
 * @return uint64_t
 */
uint64_t vfs_register_filesystem(struct vfs_filesystem_type_t *fs)
{
    struct vfs_filesystem_type_t *p = NULL;
    for (p = &vfs_fs; p; p = p->next)
    {
        if (!strcmp(p->name, fs->name)) // 已经注册相同名称的文件系统
            return VFS_E_FS_EXISTED;
    }

    fs->next = vfs_fs.next;
    vfs_fs.next = fs;
    return VFS_SUCCESS;
}

uint64_t vfs_unregister_filesystem(struct vfs_filesystem_type_t *fs)
{
    struct vfs_filesystem_type_t *p = &vfs_fs;
    while (p->next)
    {
        if (p->next == fs)
        {
            p->next = p->next->next;
            fs->next = NULL;
            return VFS_SUCCESS;
        }
        else
            p = p->next;
    }
    return VFS_E_FS_NOT_EXIST;
}

/**
 * @brief 按照路径查找文件
 *
 * @param path 路径
 * @param flags 1：返回父目录项， 0：返回结果目录项
 * @return struct vfs_dir_entry_t* 目录项
 */
struct vfs_dir_entry_t *vfs_path_walk(char *path, uint64_t flags)
{

    struct vfs_dir_entry_t *parent = vfs_root_sb->root;
    // 去除路径前的斜杠
    while (*path == '/')
        ++path;

    if ((!*path) || (*path == '\0'))
        return parent;

    struct vfs_dir_entry_t *dentry;

    while (true)
    {
        // 提取出下一级待搜索的目录名或文件名，并保存在dEntry_name中
        char *tmp_path = path;
        while ((*path && *path != '\0') && (*path != '/'))
            ++path;
        int tmp_path_len = path - tmp_path;

        dentry = (struct vfs_dir_entry_t *)kmalloc(sizeof(struct vfs_dir_entry_t), 0);
        memset(dentry, 0, sizeof(struct vfs_dir_entry_t));
        // 为目录项的名称分配内存
        dentry->name = (char *)kmalloc(tmp_path_len + 1, 0);
        // 貌似这里不需要memset，因为空间会被覆盖
        // memset(dentry->name, 0, tmp_path_len+1);

        memcpy(dentry->name, tmp_path, tmp_path_len);
        dentry->name[tmp_path_len] = '\0';
        dentry->name_length = tmp_path_len;

        if (parent->dir_inode->inode_ops->lookup(parent->dir_inode, dentry) == NULL)
        {
            // 搜索失败
            kerror("cannot find the file/dir : %s", dentry->name);
            kfree(dentry->name);
            kfree(dentry);
            return NULL;
        }
        // 找到子目录项
        // 初始化子目录项的entry
        list_init(&dentry->child_node_list);
        list_init(&dentry->subdirs_list);
        dentry->parent = parent;

        while (*path == '/')
            ++path;

        if ((!*path) || (*path == '\0')) //  已经到达末尾
        {
            if (flags & 1) // 返回父目录
            {
                return parent;
            }

            return dentry;
        }

        parent = dentry;
    }
}