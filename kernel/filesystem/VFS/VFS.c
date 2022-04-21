#include "VFS.h"
#include <common/kprint.h>

// 为filesystem_type_t结构体实例化一个链表头
static struct vfs_filesystem_type_t vfs_fs = {"filesystem", 0};


/**
 * @brief 挂载文件系统
 *
 * @param name 文件系统名
 * @param DPTE 分区表entry
 * @param DPT_type 分区表类型
 * @param buf 缓存去
 * @return struct vfs_superblock_t*
 */
struct vfs_superblock_t *vfs_mount_fs(char *name, void *DPTE, uint8_t DPT_type, void *buf)
{

    struct vfs_filesystem_type_t *p = NULL;
    for (p = &vfs_fs; p; p = p->next)
    {
        if (!strcmp(p->name, name)) // 存在符合的文件系统
        {
            return p->read_superblock(DPTE, DPT_type, buf);
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
    for(p = &vfs_fs; p;p = p->next)
    {
        if(!strcmp(p->name,fs->name))   // 已经注册相同名称的文件系统
            return VFS_E_FS_EXISTED;
    }

    fs->next = vfs_fs.next;
    vfs_fs.next = fs;
    return VFS_SUCCESS;
}

uint64_t vfs_unregister_filesystem(struct vfs_filesystem_type_t *fs)
{
    struct vfs_filesystem_type_t *p = &vfs_fs;
    while(p->next)
    {
        if(p->next == fs)
        {
            p->next = p->next->next;
            fs->next = NULL;
            return VFS_SUCCESS;
        }
        else p = p->next;
    }
    return VFS_E_FS_NOT_EXIST;
}