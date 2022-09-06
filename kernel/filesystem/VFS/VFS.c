#include "VFS.h"
#include "mount.h"
#include <common/kprint.h>
#include <common/dirent.h>
#include <common/string.h>
#include <common/errno.h>
#include <mm/mm.h>
#include <mm/slab.h>
#include <process/ptrace.h>
#include <process/process.h>

// todo: devfs完善后，删除这个
extern struct vfs_file_operations_t ps2_keyboard_fops;

// 为filesystem_type_t结构体实例化一个链表头
static struct vfs_filesystem_type_t vfs_fs = {"filesystem", 0};
struct vfs_superblock_t *vfs_root_sb = NULL;

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
    // todo: 选择挂载点
    // 判断挂载点是否存在
    struct vfs_dir_entry_t *target_dentry = NULL;
    // 由于目前还没有rootfs，因此挂载根目录时，不需要path walk
    if (strcmp(path, "/") != 0)
    {
        target_dentry = vfs_path_walk(path, 0);
        if (target_dentry == NULL)
            return NULL;
    }

    struct vfs_filesystem_type_t *p = NULL;
    for (p = &vfs_fs; p; p = p->next)
    {
        if (!strcmp(p->name, name)) // 存在符合的文件系统
        {
            struct vfs_superblock_t *sb = p->read_superblock(blk);
            if (strcmp(path, "/") == 0) // 如果挂载到的是'/'挂载点，则让其成为最顶层的文件系统
                vfs_root_sb = sb;
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
        char *tmp_path = path;
        while ((*path && *path != '\0') && (*path != '/'))
            ++path;
        int tmp_path_len = path - tmp_path;
        // 搜索是否有dentry缓存
        {
            char bk = *(tmp_path + tmp_path_len);
            *(tmp_path + tmp_path_len) = '\0';
            kdebug("to search:%s", tmp_path);
            dentry = vfs_search_dentry_list(parent, tmp_path);
            kdebug("search done, dentry=%#018lx", dentry);
            *(tmp_path + tmp_path_len) = bk;
        }

        // 如果没有找到dentry缓存，则申请新的dentry
        if (dentry == NULL)
        {
            dentry = (struct vfs_dir_entry_t *)kzalloc(sizeof(struct vfs_dir_entry_t), 0);
            // 为目录项的名称分配内存
            dentry->name = (char *)kmalloc(tmp_path_len + 1, 0);
            // 貌似这里不需要memset，因为空间会被覆盖
            // memset(dentry->name, 0, tmp_path_len+1);

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
            // 初始化子目录项的entry
            list_init(&dentry->child_node_list);
            list_init(&dentry->subdirs_list);
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
 * @param path(r8) 路径
 * @param mode(r9) 模式
 * @return uint64_t
 */
uint64_t sys_mkdir(struct pt_regs *regs)
{
    const char *path = (const char *)regs->r8;
    // kdebug("path = %s", path);
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
    // kdebug("to walk: %s", buf);
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

    // kdebug("last_slash=%d", last_slash);
    // kdebug("name=%s", path + last_slash + 1);
    subdir_dentry->parent = parent_dir;
    // kdebug("to mkdir, parent name=%s", parent_dir->name);
    int retval = parent_dir->dir_inode->inode_ops->mkdir(parent_dir->dir_inode, subdir_dentry, 0);
    list_add(&parent_dir->subdirs_list, &subdir_dentry->child_node_list);
    // kdebug("retval = %d", retval);
    return 0;
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
    char *path = (char *)kmalloc(path_len, 0);
    if (path == NULL)
        return -ENOMEM;
    memset(path, 0, path_len);

    strncpy_from_user(path, filename, path_len);
    // 去除末尾的 '/'
    if (path_len >= 2 && path[path_len - 2] == '/')
    {
        path[path_len - 2] = '\0';
        --path_len;
    }

    // 寻找文件
    struct vfs_dir_entry_t *dentry = vfs_path_walk(path, 0);

    // if (dentry != NULL)
    //     printk_color(ORANGE, BLACK, "Found %s\nDIR_FstClus:%#018lx\tDIR_FileSize:%#018lx\n", path, ((struct fat32_inode_info_t *)(dentry->dir_inode->private_inode_info))->first_clus, dentry->dir_inode->file_size);
    // else
    //     printk_color(ORANGE, BLACK, "Can`t find file\n");
    // kdebug("flags=%#018lx", flags);
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
        dentry = (struct vfs_dir_entry_t *)kmalloc(sizeof(struct vfs_dir_entry_t), 0);
        memset(dentry, 0, sizeof(struct vfs_dir_entry_t));

        dentry->name_length = path_len - tmp_index - 1;
        dentry->name = (char *)kmalloc(dentry->name_length, 0);
        memset(dentry->name, 0, dentry->name_length);
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
    if ((flags & O_DIRECTORY) && (dentry->dir_inode->attribute != VFS_ATTR_DIR))
        return -ENOTDIR;

    // // 要找的目标是文件夹
    // if ((flags & O_DIRECTORY) && dentry->dir_inode->attribute == VFS_ATTR_DIR)
    //     return -EISDIR;

    // // todo: 引入devfs后删除这段代码
    // // 暂时遇到设备文件的话，就将其first clus设置为特定值
    // if (path_len >= 5 && filename[0] == '/' && filename[1] == 'd' && filename[2] == 'e' && filename[3] == 'v' && filename[4] == '/')
    // {
    //     if (dentry->dir_inode->attribute & VFS_ATTR_FILE)
    //     {
    //         // 对于fat32文件系统上面的设备文件，设置其起始扇区
    //         ((struct fat32_inode_info_t *)(dentry->dir_inode->private_inode_info))->first_clus |= 0xf0000000;
    //         dentry->dir_inode->sb->sb_ops->write_inode(dentry->dir_inode);
    //         dentry->dir_inode->attribute |= VFS_ATTR_DEVICE;
    //     }
    // }

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
    dentry->name = (char *)kzalloc(name_size, 0);
    list_init(&dentry->child_node_list);
    list_init(&dentry->subdirs_list);
    return dentry;
}

/**
 * @brief 初始化vfs
 *
 * @return int 错误码
 */
int vfs_init()
{
    mount_init();
    return 0;
}