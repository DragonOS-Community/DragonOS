#include "devfs.h"
#include <filesystem/VFS/VFS.h>
#include <common/glib.h>
#include <common/string.h>
#include <mm/slab.h>

static struct vfs_super_block_operations_t devfs_sb_ops;
static struct vfs_dir_entry_operations_t devfs_dentry_ops;
static struct vfs_file_operations_t devfs_file_ops;
static struct vfs_inode_operations_t devfs_inode_ops;

static struct vfs_dir_entry_t *devfs_root_dentry; // 根结点的dentry
static struct vfs_superblock_t devfs_sb = {0};

extern struct vfs_file_operations_t ps2_keyboard_fops;

/**
 * @brief 创建devfs的super block
 *
 * @param blk 未使用（devfs为伪文件系统，不需要物理设备）
 * @return struct vfs_superblock_t*
 */
struct vfs_superblock_t *devfs_read_superblock(struct block_device *blk)
{
    devfs_sb.blk_device = NULL;
    devfs_sb.root = devfs_root_dentry;
    devfs_sb.sb_ops = &devfs_sb_ops;
    devfs_sb.dir_ops = &devfs_dentry_ops;
    // todo: 为devfs增加私有信息
    devfs_sb.private_sb_info = NULL;
    kdebug("devfs read superblock done");
    return &devfs_sb;
}

static void devfs_write_superblock(struct vfs_superblock_t *sb) {}

static void devfs_put_superblock(struct vfs_superblock_t *sb) {}

static void devfs_write_inode(struct vfs_index_node_t *inode) {}
static struct vfs_super_block_operations_t devfs_sb_ops =
    {
        .write_superblock = &devfs_write_superblock,
        .put_superblock = &devfs_put_superblock,
        .write_inode = &devfs_write_inode,
};

static long devfs_compare(struct vfs_dir_entry_t *parent_dEntry, char *source_filename, char *dest_filename) {}

static long devfs_hash(struct vfs_dir_entry_t *dEntry, char *filename) {}

static long devfs_release(struct vfs_dir_entry_t *dEntry) {}

static long devfs_iput(struct vfs_dir_entry_t *dEntry, struct vfs_index_node_t *inode) {}

static struct vfs_dir_entry_operations_t devfs_dentry_ops =
    {
        .compare = &devfs_compare,
        .hash = &devfs_hash,
        .release = &devfs_release,
        .iput = &devfs_iput,
};

static long devfs_open(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr) { return 0; }
static long devfs_close(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr) {}
static long devfs_read(struct vfs_file_t *file_ptr, char *buf, int64_t count, long *position) {}
static long devfs_write(struct vfs_file_t *file_ptr, char *buf, int64_t count, long *position) {}
static long devfs_lseek(struct vfs_file_t *file_ptr, long offset, long origin) {}
static long devfs_ioctl(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr, uint64_t cmd, uint64_t arg) {return 0;}
static long devfs_readdir(struct vfs_file_t *file_ptr, void *dirent, vfs_filldir_t filler) {return 0;}

static struct vfs_file_operations_t devfs_file_ops =
    {
        .open = &devfs_open,
        .close = &devfs_close,
        .read = &devfs_read,
        .write = &devfs_write,
        .lseek = &devfs_lseek,
        .ioctl = &devfs_ioctl,
        .readdir = &devfs_readdir,
};

/**
 * @brief 创建新的文件
 * @param parent_inode 父目录的inode结构体
 * @param dest_dEntry 新文件的dentry
 * @param mode 创建模式
 */
static long devfs_create(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dest_dEntry, int mode)
{
}
static struct vfs_dir_entry_t *devfs_lookup(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dest_dEntry)
{
    kdebug("devfs_lookup: %s", dest_dEntry->name);
    return NULL;
}
static long devfs_mkdir(struct vfs_index_node_t *inode, struct vfs_dir_entry_t *dEntry, int mode) {}
static long devfs_rmdir(struct vfs_index_node_t *inode, struct vfs_dir_entry_t *dEntry) {}
static long devfs_rename(struct vfs_index_node_t *old_inode, struct vfs_dir_entry_t *old_dEntry, struct vfs_index_node_t *new_inode, struct vfs_dir_entry_t *new_dEntry) {}
static long devfs_getAttr(struct vfs_dir_entry_t *dEntry, uint64_t *attr) {}
static long devfs_setAttr(struct vfs_dir_entry_t *dEntry, uint64_t *attr) {}
static struct vfs_inode_operations_t devfs_inode_ops = {
    .create = &devfs_create,
    .lookup = &devfs_lookup,
    .mkdir = &devfs_mkdir,
    .rmdir = &devfs_rmdir,
    .rename = &devfs_rename,
    .getAttr = &devfs_getAttr,
    .setAttr = &devfs_setAttr,
};

static struct vfs_filesystem_type_t devfs_fs_type =
    {
        .name = "DEVFS",
        .fs_flags = 0,
        .read_superblock = devfs_read_superblock,
        .next = NULL,
};

static __always_inline void __devfs_init_root_inode()
{
    devfs_root_dentry->dir_inode->file_ops = &devfs_file_ops;
    devfs_root_dentry->dir_inode->inode_ops = &devfs_inode_ops;

    // todo: 增加private inode info
    devfs_root_dentry->dir_inode->private_inode_info = NULL;
    devfs_root_dentry->dir_inode->sb = &devfs_sb;
}
/**
 * @brief 初始化devfs的根dentry
 */
static __always_inline void __devfs_init_root_dentry()
{
    devfs_root_dentry = (struct vfs_dir_entry_t *)kzalloc(sizeof(struct vfs_dir_entry_t), 0);
    list_init(&devfs_root_dentry->child_node_list);
    list_init(&devfs_root_dentry->subdirs_list);
    devfs_root_dentry->dir_ops = &devfs_dentry_ops;
    devfs_root_dentry->dir_inode = (struct vfs_index_node_t *)kzalloc(sizeof(struct vfs_index_node_t), 0);
    __devfs_init_root_inode();
}

int devfs_register_device()
{
    char name[] = "keyboard.dev";
    struct vfs_dir_entry_t *dentry = vfs_alloc_dentry(sizeof(name));
    strcpy(dentry->name, name);
    dentry->name_length = strlen(name);
    dentry->dir_inode = (struct vfs_index_node_t *)kzalloc(sizeof(struct vfs_index_node_t), 0);
    dentry->dir_ops = &devfs_dentry_ops;
    dentry->dir_inode->file_ops = &ps2_keyboard_fops;
    dentry->dir_inode->inode_ops = &devfs_inode_ops;
    dentry->dir_inode->private_inode_info = NULL; // todo:
    dentry->dir_inode->sb = &devfs_sb;
    dentry->parent = devfs_root_dentry;
    list_init(&dentry->child_node_list);
    list_init(&dentry->subdirs_list);
    list_append(&devfs_root_dentry->subdirs_list, &dentry->child_node_list);
    kdebug("add dev: %s", dentry->name);
    // devfs_create(&devfs_root_dentry->dir_inode, dentry->dir_inode, 0);
}
/**
 * @brief 初始化devfs
 *
 */
void devfs_init()
{
    __devfs_init_root_dentry();
    vfs_register_filesystem(&devfs_fs_type);
    vfs_mount_fs("/dev", "DEVFS", NULL);

    devfs_register_device();
}