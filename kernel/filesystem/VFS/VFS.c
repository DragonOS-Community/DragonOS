#include "VFS.h"
#include "mount.h"
#include "internal.h"
#include <common/kprint.h>
#include <debug/bug.h>
#include <common/dirent.h>
#include <common/string.h>
#include <common/errno.h>
#include <mm/mm.h>
#include <mm/slab.h>
#include <process/ptrace.h>
#include <process/process.h>
#include <filesystem/rootfs/rootfs.h>

// 为filesystem_type_t结构体实例化一个链表头
static struct vfs_filesystem_type_t vfs_fs = {"filesystem", 0};
struct vfs_superblock_t *vfs_root_sb = NULL;

struct vfs_dir_entry_t *vfs_alloc_dentry(const int name_size);

/**
 * @brief 挂载文件系统
 *
 * @param path 要挂载到的路径
 * @param name 文件系统名
 * @param blk 块设备结构体
 * @return struct vfs_superblock_t* 挂载后，文件系统的超级块
 */
struct vfs_superblock_t *vfs_mount_fs(const char *path, char *name, struct block_device *blk)
{

    // 判断挂载点是否存在
    struct vfs_dir_entry_t *target_dentry = NULL;

    target_dentry = vfs_path_walk(path, 0);
    if (target_dentry == NULL)
        return NULL;

    struct vfs_filesystem_type_t *p = NULL;
    for (p = &vfs_fs; p; p = p->next)
    {
        if (!strcmp(p->name, name)) // 存在符合的文件系统
        {
            struct vfs_superblock_t *sb = p->read_superblock(blk);
            if (strcmp(path, "/") == 0) // 如果挂载到的是'/'挂载点，则让其成为最顶层的文件系统
            {
                vfs_root_sb = sb;
            }
            else
            {
                kdebug("to mount %s", name);
                // 调用mount机制，挂载文件系统
                struct vfs_dir_entry_t *new_dentry = sb->root;
                // 注意，umount的时候需要释放这些内存
                new_dentry->name = kzalloc(target_dentry->name_length + 1, 0);
                new_dentry->name_length = target_dentry->name_length;

                do_mount(target_dentry, new_dentry);
            }
            return sb;
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
 * @brief 在dentry的sub_dir_list中搜索指定名称的dentry
 *
 * @param dentry 目录项结构体dentry
 * @param name 待搜索的dentry名称
 * @return struct vfs_dir_entry_t* 目标dentry （无结果则返回NULL）
 */
static struct vfs_dir_entry_t *vfs_search_dentry_list(struct vfs_dir_entry_t *dentry, const char *name)
{
    if (list_empty(&dentry->subdirs_list))
        return NULL;

    struct List *ptr = &dentry->subdirs_list;
    struct vfs_dir_entry_t *d_ptr = NULL;
    do
    {
        ptr = list_next(ptr);
        d_ptr = container_of(ptr, struct vfs_dir_entry_t, child_node_list);
        if (strcmp(name, d_ptr->name) == 0)
            return d_ptr;
    } while (list_next(ptr) != (&dentry->subdirs_list));

    return NULL;
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

    struct vfs_dir_entry_t *dentry = NULL;
    // kdebug("path before walk:%s", path);
    while (true)
    {
        // 提取出下一级待搜索的目录名或文件名，并保存在dEntry_name中
        const char *tmp_path = path;
        while ((*path && *path != '\0') && (*path != '/'))
            ++path;
        int tmp_path_len = path - tmp_path;
        // 搜索是否有dentry缓存
        {
            char *tmpname = kzalloc(tmp_path_len + 1, 0);
            strncpy(tmpname, tmp_path, tmp_path_len);
            tmpname[tmp_path_len] = '\0';

            dentry = vfs_search_dentry_list(parent, tmpname);

            kfree(tmpname);
        }

        // 如果没有找到dentry缓存，则申请新的dentry
        if (dentry == NULL)
        {
            dentry = vfs_alloc_dentry(tmp_path_len + 1);

            memcpy(dentry->name, (void *)tmp_path, tmp_path_len);
            dentry->name[tmp_path_len] = '\0';
            // kdebug("tmp_path_len=%d, dentry->name= %s", tmp_path_len, dentry->name);
            dentry->name_length = tmp_path_len;

            if (parent->dir_inode->inode_ops->lookup(parent->dir_inode, dentry) == NULL)
            {
                // 搜索失败
                // kerror("cannot find the file/dir : %s", dentry->name);
                kfree(dentry->name);
                kfree(dentry);
                return NULL;
            }
            // 找到子目录项
            dentry->parent = parent;

            list_add(&parent->subdirs_list, &dentry->child_node_list);
        }

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
 * @return dirent的总大小
 */
int vfs_fill_dirent(void *buf, ino_t d_ino, char *name, int namelen, unsigned char type, off_t offset)
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
 * @param path 文件夹路径
 * @param mode 创建模式
 * @param from_userland 该创建请求是否来自用户态
 * @return int64_t 错误码
 */
int64_t vfs_mkdir(const char *path, mode_t mode, bool from_userland)
{
    uint32_t pathlen;
    if (from_userland)
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
        return -ENOTDIR;

    char *buf = (char *)kzalloc(last_slash + 2, 0);

    // 拷贝字符串（不包含要被创建的部分）
    if (from_userland)
        strncpy_from_user(buf, path, last_slash);
    else
        strncpy(buf, path, last_slash);
    buf[last_slash + 1] = '\0';

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
        return -EEXIST;
    }

    struct vfs_dir_entry_t *subdir_dentry = (struct vfs_dir_entry_t *)kzalloc(sizeof(struct vfs_dir_entry_t), 0);

    list_init(&subdir_dentry->subdirs_list);
    list_init(&subdir_dentry->child_node_list);
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

    // kdebug("last_slash=%d", last_slash);
    // kdebug("name=%s", path + last_slash + 1);
    subdir_dentry->parent = parent_dir;
    // kdebug("to mkdir, parent name=%s", parent_dir->name);
    int retval = parent_dir->dir_inode->inode_ops->mkdir(parent_dir->dir_inode, subdir_dentry, 0);
    list_append(&parent_dir->subdirs_list, &subdir_dentry->child_node_list);
    // kdebug("retval = %d", retval);
    return 0;
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
    // kdebug("path = %s", path);
    mode_t mode = (mode_t)regs->r9;

    if (regs->cs & USER_CS)
        return vfs_mkdir(path, mode, true);
    else
        return vfs_mkdir(path, mode, false);
}

/**
 * @brief 打开文件
 *
 * @param filename 文件路径
 * @param flags 标志位
 * @return uint64_t 错误码
 */
uint64_t do_open(const char *filename, int flags)
{
    long path_len = strnlen_user(filename, PAGE_4K_SIZE) + 1;

    if (path_len <= 0) // 地址空间错误
    {
        return -EFAULT;
    }
    else if (path_len >= PAGE_4K_SIZE) // 名称过长
    {
        return -ENAMETOOLONG;
    }

    // 为待拷贝文件路径字符串分配内存空间
    char *path = (char *)kzalloc(path_len, 0);
    if (path == NULL)
        return -ENOMEM;

    strncpy_from_user(path, filename, path_len);
    // 去除末尾的 '/'
    if (path_len >= 2 && path[path_len - 2] == '/')
    {
        path[path_len - 2] = '\0';
        --path_len;
    }

    // 寻找文件
    struct vfs_dir_entry_t *dentry = vfs_path_walk(path, 0);

    if (dentry == NULL && flags & O_CREAT)
    {
        // 先找到倒数第二级目录
        int tmp_index = -1;
        for (int i = path_len - 1; i >= 0; --i)
        {
            if (path[i] == '/')
            {
                tmp_index = i;
                break;
            }
        }

        struct vfs_dir_entry_t *parent_dentry = NULL;
        // kdebug("tmp_index=%d", tmp_index);
        if (tmp_index > 0)
        {

            path[tmp_index] = '\0';
            dentry = vfs_path_walk(path, 0);
            if (dentry == NULL)
            {
                kfree(path);
                return -ENOENT;
            }
            parent_dentry = dentry;
        }
        else
            parent_dentry = vfs_root_sb->root;

        // 创建新的文件
        dentry = (struct vfs_dir_entry_t *)kzalloc(sizeof(struct vfs_dir_entry_t), 0);

        dentry->name_length = path_len - tmp_index - 1;
        dentry->name = (char *)kzalloc(dentry->name_length + 1, 0);
        strncpy(dentry->name, path + tmp_index + 1, dentry->name_length);
        // kdebug("to create new file:%s   namelen=%d", dentry->name, dentry->name_length)
        dentry->parent = parent_dentry;
        uint64_t retval = parent_dentry->dir_inode->inode_ops->create(parent_dentry->dir_inode, dentry, 0);
        if (retval != 0)
        {
            kfree(dentry->name);
            kfree(dentry);
            kfree(path);
            return retval;
        }

        list_init(&dentry->child_node_list);
        list_init(&dentry->subdirs_list);
        list_add(&parent_dentry->subdirs_list, &dentry->child_node_list);
        // kdebug("created.");
    }
    kfree(path);
    if (dentry == NULL)
        return -ENOENT;

    // 要求打开文件夹而目标不是文件夹
    if ((flags & O_DIRECTORY) && (dentry->dir_inode->attribute != VFS_IF_DIR))
        return -ENOTDIR;

    // 创建文件描述符
    struct vfs_file_t *file_ptr = (struct vfs_file_t *)kzalloc(sizeof(struct vfs_file_t), 0);

    int errcode = -1;

    file_ptr->dEntry = dentry;
    file_ptr->mode = flags;

    file_ptr->file_ops = dentry->dir_inode->file_ops;

    // 如果文件系统实现了打开文件的函数
    if (file_ptr->file_ops && file_ptr->file_ops->open)
        errcode = file_ptr->file_ops->open(dentry->dir_inode, file_ptr);

    if (errcode != VFS_SUCCESS)
    {
        kfree(file_ptr);
        return -EFAULT;
    }

    if (file_ptr->mode & O_TRUNC) // 清空文件
        file_ptr->dEntry->dir_inode->file_size = 0;

    if (file_ptr->mode & O_APPEND)
        file_ptr->position = file_ptr->dEntry->dir_inode->file_size;
    else
        file_ptr->position = 0;

    struct vfs_file_t **f = current_pcb->fds;

    int fd_num = -1;

    // 在指针数组中寻找空位
    // todo: 当pcb中的指针数组改为动态指针数组之后，需要更改这里（目前还是静态指针数组）
    for (int i = 0; i < PROC_MAX_FD_NUM; ++i)
    {
        if (f[i] == NULL) // 找到指针数组中的空位
        {
            fd_num = i;
            break;
        }
    }

    // 指针数组没有空位了
    if (fd_num == -1)
    {
        kfree(file_ptr);
        return -EMFILE;
    }
    // 保存文件描述符
    f[fd_num] = file_ptr;
    return fd_num;
}

uint64_t sys_open(struct pt_regs *regs)
{
    char *filename = (char *)(regs->r8);
    int flags = (int)(regs->r9);

    return do_open(filename, flags);
}

/**
 * @brief 动态分配dentry以及路径字符串名称
 *
 * @param name_size 名称字符串大小（字节）(注意考虑字符串最后需要有一个‘\0’作为结尾)
 * @return struct vfs_dir_entry_t* 创建好的dentry
 */
struct vfs_dir_entry_t *vfs_alloc_dentry(const int name_size)
{
    if (unlikely(name_size > VFS_MAX_PATHLEN))
        return NULL;
    struct vfs_dir_entry_t *dentry = (struct vfs_dir_entry_t *)kzalloc(sizeof(struct vfs_dir_entry_t), 0);
    if (unlikely(dentry == NULL))
        return NULL;
    dentry->name = (char *)kzalloc(name_size, 0);
    list_init(&dentry->child_node_list);
    list_init(&dentry->subdirs_list);
    return dentry;
}

/**
 * @brief 判断是否可以删除指定的dentry
 *
 * 1、我们不能删除一个只读的dentry
 * 2、我们应当对这个dentry的inode拥有写、执行权限（暂时还没有实现权限）
 * 3、如果dentry指向的是文件夹，而isdir为false，则不能删除
 * 3、如果dentry指向的是文件，而isdir为true，则不能删除
 * @param dentry 将要被删除的dentry
 * @param isdir 是否要删除文件夹
 * @return int 错误码
 */
int vfs_may_delete(struct vfs_dir_entry_t *dentry, bool isdir)
{
    // 当dentry没有inode的时候，认为是bug
    BUG_ON(dentry->dir_inode == NULL);

    // todo: 进行权限检查

    if (isdir) // 要删除文件夹
    {
        if (!D_ISDIR(dentry))
            return -ENOTDIR;
        else if (IS_ROOT(dentry))
            return -EBUSY;
    }
    else if (D_ISDIR(dentry)) // 要删除文件但是当前是文件夹
        return -EISDIR;

    return 0;
}

/**
 * @brief 删除文件夹
 *
 * @param path 文件夹路径
 * @param from_userland 请求是否来自用户态
 * @return int64_t 错误码
 */
int64_t vfs_rmdir(const char *path, bool from_userland)
{
    uint32_t pathlen;
    if (from_userland)
        pathlen = strnlen_user(path, PAGE_4K_SIZE - 1);
    else
        pathlen = strnlen(path, PAGE_4K_SIZE - 1);

    if (pathlen == 0)
        return -ENOENT;

    int last_slash = -1;

    // 去除末尾的'/'
    for (int i = pathlen - 1; i >= 0; --i)
    {
        if (path[i] != '/')
        {
            last_slash = i + 1;
            break;
        }
    }

    // 路径格式不合法
    if (last_slash < 0)
        return -ENOTDIR;
    else if (path[0] != '/')
        return -EINVAL;

    char *buf = (char *)kzalloc(last_slash + 2, 0);

    // 拷贝字符串（不包含要被创建的部分）
    if (from_userland)
        strncpy_from_user(buf, path, last_slash);
    else
        strncpy(buf, path, last_slash);
    buf[last_slash + 1] = '\0';

    struct vfs_dir_entry_t *dentry = vfs_path_walk(buf, 0);

    if (dentry == NULL)
        return -ENOENT;

    int retval = vfs_may_delete(dentry, true);
    if (retval != 0)
        return retval;
    // todo: 对dentry和inode加锁
    retval = -EBUSY;
    if (is_local_mountpoint(dentry))
        goto out;
    // todo:
    retval = dentry->dir_inode->inode_ops->rmdir(dentry->dir_inode, dentry);
    if (retval != 0)
        goto out;

    dentry->dir_inode->attribute |= VFS_IF_DEAD; // 将当前inode标记为dead
    dont_mount(dentry);                          // 将当前dentry标记为不可被挂载
    detach_mounts(dentry);                       // 清理同样挂载在该路径的所有挂载点的挂载树

    vfs_dentry_put(dentry); // 释放dentry
out:;
    // todo: 对dentry和inode放锁
    return retval;
}

/**
 * @brief 删除文件夹的系统调用函数
 *
 * @param r8 文件夹路径
 * @return uint64_t 错误码
 */
uint64_t sys_rmdir(struct pt_regs *regs)
{
    if (SYSCALL_FROM_USER(regs))
        return vfs_rmdir((char *)regs->r8, true);
    else
        return vfs_rmdir((char *)regs->r8, false);
}

/**
 * @brief 分配inode并将引用计数初始化为1
 *
 * @return struct vfs_index_node_t * 分配得到的inode
 */
struct vfs_index_node_t *vfs_alloc_inode()
{
    struct vfs_index_node_t *inode = kzalloc(sizeof(struct vfs_index_node_t), 0);
    inode->ref_count = 1; // 初始化引用计数为1
    return inode;
}

/**
 * @brief 初始化vfs
 *
 * @return int 错误码
 */
int vfs_init()
{
    mount_init();
    rootfs_init();
    return 0;
}