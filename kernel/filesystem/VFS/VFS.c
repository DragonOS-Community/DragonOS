#include "VFS.h"
#include <common/kprint.h>
#include <common/dirent.h>
#include <common/errno.h>
#include <mm/mm.h>
#include <mm/slab.h>
#include <process/ptrace.h>
#include <process/process.h>

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
struct vfs_dir_entry_t *vfs_path_walk(const char *path, uint64_t flags)
{

    struct vfs_dir_entry_t *parent = vfs_root_sb->root;
    // 去除路径前的斜杠
    while (*path == '/')
        ++path;

    if ((!*path) || (*path == '\0'))
        return parent;

    struct vfs_dir_entry_t *dentry;
    kdebug("path before walk:%s", path);
    while (true)
    {
        // 提取出下一级待搜索的目录名或文件名，并保存在dEntry_name中
        const char *tmp_path = path;
        while ((*path && *path != '\0') && (*path != '/'))
            ++path;
        int tmp_path_len = path - tmp_path;
        dentry = (struct vfs_dir_entry_t *)kmalloc(sizeof(struct vfs_dir_entry_t), 0);
        memset(dentry, 0, sizeof(struct vfs_dir_entry_t));
        // 为目录项的名称分配内存
        dentry->name = (char *)kmalloc(tmp_path_len + 1, 0);
        // 貌似这里不需要memset，因为空间会被覆盖
        // memset(dentry->name, 0, tmp_path_len+1);

        memcpy(dentry->name, (void *)tmp_path, tmp_path_len);
        dentry->name[tmp_path_len] = '\0';
        kdebug("tmp_path_len=%d, dentry->name= %s", tmp_path_len, dentry->name);
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

        list_add(&parent->subdirs_list, &dentry->child_node_list);
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

/**
 * @brief 填充dentry
 *
 */
int vfs_fill_dentry(void *buf, ino_t d_ino, char *name, int namelen, unsigned char type, off_t offset)
{
    struct dirent *dent = (struct dirent *)buf;

    // 如果尝试访问内核空间，则返回错误
    if (!(verify_area((uint64_t)buf, sizeof(struct dirent) + namelen)))
        return -EFAULT;

    // ====== 填充dirent结构体 =====
    memset(buf, 0, sizeof(struct dirent) + namelen);

    memcpy(dent->d_name, name, namelen);
    dent->d_name[namelen] = '\0';
    // 暂时不显示目录下的记录数
    dent->d_reclen = 0;
    dent->d_ino = d_ino;
    dent->d_off = offset;
    dent->d_type = type;

    // 返回dirent的总大小
    return sizeof(struct dirent) + namelen;
}

/**
 * @brief 创建文件夹
 *
 * @param path(r8) 路径
 * @param mode(r9) 模式
 * @return uint64_t
 */
uint64_t sys_mkdir(struct pt_regs *regs)
{
    const char *path = (const char *)regs->r8;
    kdebug("path = %s", path);
    mode_t mode = (mode_t)regs->r9;
    uint32_t pathlen;
    if (regs->cs & USER_CS)
        pathlen = strnlen_user(path, PAGE_4K_SIZE - 1);
    else
        pathlen = strnlen(path, PAGE_4K_SIZE - 1);

    if (pathlen == 0)
        return -ENOENT;

    int last_slash = -1;

    // 查找最后一个'/'，忽略路径末尾的'/'
    for (int i = pathlen - 2; i >= 0; --i)
    {
        if (path[i] == '/')
        {
            last_slash = i;
            break;
        }
    }

    // 路径格式不合法（必须使用绝对路径）
    if (last_slash < 0)
        return ENOTDIR;

    char *buf = (char *)kmalloc(last_slash + 1, 0);
    memset(buf, 0, pathlen + 1);

    // 拷贝字符串（不包含要被创建的部分）
    if (regs->cs & USER_CS)
        strncpy_from_user(buf, path, last_slash);
    else
        strncpy(buf, path, last_slash);
    buf[last_slash + 1] = '\0';
    kdebug("to walk: %s", buf);
    // 查找父目录
    struct vfs_dir_entry_t *parent_dir = vfs_path_walk(buf, 0);

    if (parent_dir == NULL)
    {
        kwarn("parent dir is NULL.");
        kfree(buf);
        return -ENOENT;
    }
    kfree(buf);

    // 检查父目录中是否已经有相同的目录项
    if (vfs_path_walk((const char *)path, 0) != NULL)
    {
        // 目录中已有对应的文件夹
        kwarn("Dir '%s' aleardy exists.", path);
        kdebug("name = %s", vfs_path_walk((const char *)path, 0)->name) return -EEXIST;
    }

    struct vfs_dir_entry_t *subdir_dentry = (struct vfs_dir_entry_t *)kmalloc(sizeof(struct vfs_dir_entry_t), 0);
    memset((void *)subdir_dentry, 0, sizeof(struct vfs_dir_entry_t));

    if (path[pathlen - 1] == '/')
        subdir_dentry->name_length = pathlen - last_slash - 2;
    else
        subdir_dentry->name_length = pathlen - last_slash - 1;
    subdir_dentry->name = (char *)kmalloc(subdir_dentry->name_length + 1, 0);
    memset((void *)subdir_dentry->name, 0, subdir_dentry->name_length + 1);

    for (int i = last_slash + 1, cnt = 0; i < pathlen && cnt < subdir_dentry->name_length; ++i, ++cnt)
    {
        subdir_dentry->name[cnt] = path[i];
    }
    ++subdir_dentry->name_length;

    kdebug("last_slash=%d", last_slash);
    kdebug("name=%s", path + last_slash + 1);
    subdir_dentry->parent = parent_dir;
    kdebug("to mkdir, parent name=%s", parent_dir->name);
    int retval = parent_dir->dir_inode->inode_ops->mkdir(parent_dir->dir_inode, subdir_dentry, 0);
    list_add(&parent_dir->subdirs_list, &subdir_dentry->child_node_list);
    kdebug("retval = %d", retval);
    return 0;
}