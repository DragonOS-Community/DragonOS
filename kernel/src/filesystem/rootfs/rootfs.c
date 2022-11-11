#include "rootfs.h"
#include <filesystem/VFS/VFS.h>
#include <common/string.h>
#include <filesystem/VFS/mount.h>

static struct vfs_superblock_t rootfs_sb = {0};
extern struct vfs_superblock_t *vfs_root_sb;

/**
 * @brief 释放dentry本身所占的内存
 *
 * @param dentry
 */
static inline void __release_dentry(struct vfs_dir_entry_t *dentry)
{
    kfree(dentry->name);
    kfree(dentry);
}

struct vfs_super_block_operations_t rootfs_sb_ops = {
    .put_superblock = NULL,
    .write_inode = NULL,
    .write_superblock = NULL,
};

static struct vfs_dir_entry_t *rootfs_lookup(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dest_dEntry)
{
    return NULL;
}
struct vfs_inode_operations_t rootfs_inode_ops = {
    .create = NULL,
    .getAttr = NULL,
    .lookup = NULL,
    .lookup = &rootfs_lookup,
    .mkdir = NULL,
    .rename = NULL,
    .rmdir = NULL,
    .setAttr = NULL,
};

static long rootfs_open(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr)
{
    return 0;
}
static long rootfs_close(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr) { return 0; }
static long rootfs_read(struct vfs_file_t *file_ptr, char *buf, int64_t count, long *position) { return 0; }
static long rootfs_write(struct vfs_file_t *file_ptr, char *buf, int64_t count, long *position) { return 0; }
static long rootfs_lseek(struct vfs_file_t *file_ptr, long offset, long origin) { return 0; }
static long rootfs_ioctl(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr, uint64_t cmd, uint64_t arg) { return 0; }

static long rootfs_readdir(struct vfs_file_t *file_ptr, void *dirent, vfs_filldir_t filler)
{
    // 循环读取目录下的目录项
    struct vfs_dir_entry_t *dentry = file_ptr->dEntry;
    struct List *list = &dentry->subdirs_list;
    // 先切换到position处
    for (int i = 0; i <= file_ptr->position; ++i)
    {
        list = list_next(list);
        if (list == &dentry->subdirs_list) // 找完了
            goto failed;
    }

    // 存在目录项
    // 增加偏移量
    ++file_ptr->position;
    // 获取目标dentry（由于是子目录项，因此是child_node_list）
    struct vfs_dir_entry_t *target_dent = container_of(list, struct vfs_dir_entry_t, child_node_list);
    // kdebug("target name=%s, namelen=%d", target_dent->name, target_dent->name_length);

    char *name = (char *)kzalloc(target_dent->name_length + 1, 0);
    strncpy(name, target_dent->name, target_dent->name_length);

    uint32_t dentry_type = target_dent->dir_inode->attribute;

    return filler(dirent, file_ptr->position - 1, name, target_dent->name_length, dentry_type, file_ptr->position - 1);
failed:;
    return 0;
}

static long rootfs_compare(struct vfs_dir_entry_t *parent_dEntry, char *source_filename, char *dest_filename) { return 0; }

static long rootfs_hash(struct vfs_dir_entry_t *dEntry, char *filename) { return 0; }

static long rootfs_release(struct vfs_dir_entry_t *dEntry) { return 0; }

static long rootfs_iput(struct vfs_dir_entry_t *dEntry, struct vfs_index_node_t *inode) { return 0; }

struct vfs_dir_entry_operations_t rootfs_dentry_ops =
    {
        .compare = &rootfs_compare,
        .hash = &rootfs_hash,
        .release = &rootfs_release,
        .iput = &rootfs_iput,
};

struct vfs_file_operations_t rootfs_file_ops = {
    .open = &rootfs_open,
    .close = &rootfs_close,
    .read = &rootfs_read,
    .write = &rootfs_write,
    .lseek = &rootfs_lseek,
    .ioctl = &rootfs_ioctl,
    .readdir = &rootfs_readdir,
};

/**
 * @brief 为在rootfs下创建目录（仅仅是形式上的目录，为了支持文件系统挂载）
 *
 * @param name 目录名称
 * @return int
 */
static int rootfs_add_dir(const char *name)
{
    {
        // 检查名称重复
        struct List *list = &rootfs_sb.root->subdirs_list;
        while (list_next(list) != &rootfs_sb.root->subdirs_list)
        {
            list = list_next(list);
            struct vfs_dir_entry_t *tmp = container_of(list, struct vfs_dir_entry_t, child_node_list);
            if (strcmp(tmp->name, name) == 0)
                return -EEXIST;
        }
    }

    struct vfs_dir_entry_t *dentry = vfs_alloc_dentry(strlen(name) + 1);
    strcpy(dentry->name, name);
    dentry->name_length = strlen(name);
    dentry->parent = rootfs_sb.root;
    list_append(&rootfs_sb.root->subdirs_list, &dentry->child_node_list);
    return 0;
}

void rootfs_init()
{
    // 初始化超级块
    rootfs_sb.blk_device = NULL;
    rootfs_sb.private_sb_info = NULL;
    rootfs_sb.sb_ops = &rootfs_sb_ops;
    rootfs_sb.dir_ops = &rootfs_dentry_ops;

    // 初始化dentry
    rootfs_sb.root = vfs_alloc_dentry(sizeof("/"));
    struct vfs_dir_entry_t *dentry = rootfs_sb.root;
    strncpy(dentry->name, "/", 2);
    dentry->name_length = 1;
    dentry->parent = dentry;

    // 初始化root inode
    dentry->dir_inode = vfs_alloc_inode();
    dentry->dir_inode->sb = &rootfs_sb;
    dentry->dir_inode->inode_ops = &rootfs_inode_ops;
    dentry->dir_inode->file_ops = &rootfs_file_ops;
    dentry->dir_inode->attribute = VFS_IF_DIR;

    // 直接将vfs的根superblock设置为rootfs的超级块
    vfs_root_sb = &rootfs_sb;

    // 创建/dev等目录的dentry（以便文件系统的mount）
    if (rootfs_add_dir("dev") != 0)
        kerror("create dir 'dev' in rootfs failed");
}

/**
 * @brief 当新的根文件系统被挂载后，将原有的挂载在rootfs下的文件系统，迁移到新的根文件系统上
 *
 */
static void rootfs_migrate()
{
    kdebug("Migrating rootfs's dentries...");
    struct List *list = &rootfs_sb.root->subdirs_list;
    if (unlikely(list_empty(list)))
        return;
    list = list_next(list);
    while (list != &rootfs_sb.root->subdirs_list)
    {

        struct vfs_dir_entry_t *tmp = container_of(list, struct vfs_dir_entry_t, child_node_list);
        if (tmp->dir_inode != NULL)
        {
            list = list_next(list); // 获取下一个列表结点（不然的话下面的几行代码就覆盖掉了正确的值了）

            tmp->parent = vfs_root_sb->root;
            list_init(&tmp->child_node_list);
            list_append(&vfs_root_sb->root->subdirs_list, &tmp->child_node_list);
        }
        else
        {
            list = list_next(list); // 不迁移空的dentry，直接释放他们
            list_del(&tmp->child_node_list);
            __release_dentry(tmp);
        }
    }
}

/**
 * @brief 当磁盘文件系统被成功挂载后，释放rootfs所占的空间
 *
 */
void rootfs_umount()
{
    // 将原有的“dev”文件夹等进行迁移
    rootfs_migrate();
    kinfo("Umounting rootfs...");

    // 遍历mount链表，删除所有父目录是rootfs的dentry
    struct mountpoint *mp = NULL;
    while (1)
    {
        mp = mount_find_mnt_list_by_parent(rootfs_sb.root);
        if (mp == NULL)
            break;

        // 释放dentry（由于没有创建inode，因此不需要释放）
        __release_dentry(mp->dentry);
        // 释放mountpoint结构体
        mount_release_mountpoint(mp);
    }

    // 释放root dentry及其inode
    kfree(rootfs_sb.root->dir_inode);
    __release_dentry(rootfs_sb.root);
}