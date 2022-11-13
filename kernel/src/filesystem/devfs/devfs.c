#include "devfs.h"
#include "internal.h"
#include <filesystem/VFS/VFS.h>
#include <common/glib.h>
#include <common/string.h>
#include <mm/slab.h>
#include <common/spinlock.h>
#include <debug/bug.h>

struct vfs_super_block_operations_t devfs_sb_ops;
struct vfs_dir_entry_operations_t devfs_dentry_ops;
struct vfs_file_operations_t devfs_file_ops;
struct vfs_inode_operations_t devfs_inode_ops;

struct vfs_dir_entry_t *devfs_root_dentry; // 根结点的dentry
struct vfs_superblock_t devfs_sb = {0};
const char __devfs_mount_path[] = "/dev";

static spinlock_t devfs_global_lock; // devfs的全局锁
static uint64_t __tmp_uuid = 0;      // devfs的临时uuid变量（todo:在引入uuid lib之后删除这里）

static inline uint64_t __devfs_get_uuid()
{
    // todo : 更改为使用uuid库来生成uuid
    return ++__tmp_uuid;
}

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

static void devfs_write_superblock(struct vfs_superblock_t *sb) { return; }

static void devfs_put_superblock(struct vfs_superblock_t *sb) { return; }

static void devfs_write_inode(struct vfs_index_node_t *inode) { return; }
struct vfs_super_block_operations_t devfs_sb_ops =
    {
        .write_superblock = &devfs_write_superblock,
        .put_superblock = &devfs_put_superblock,
        .write_inode = &devfs_write_inode,
};

static long devfs_compare(struct vfs_dir_entry_t *parent_dEntry, char *source_filename, char *dest_filename) { return 0; }

static long devfs_hash(struct vfs_dir_entry_t *dEntry, char *filename) { return 0; }

static long devfs_release(struct vfs_dir_entry_t *dEntry) { return 0; }

static long devfs_iput(struct vfs_dir_entry_t *dEntry, struct vfs_index_node_t *inode) { return 0; }

struct vfs_dir_entry_operations_t devfs_dentry_ops =
    {
        .compare = &devfs_compare,
        .hash = &devfs_hash,
        .release = &devfs_release,
        .iput = &devfs_iput,
};

static long devfs_open(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr)
{
    return 0;
}
static long devfs_close(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr) { return 0; }
static long devfs_read(struct vfs_file_t *file_ptr, char *buf, int64_t count, long *position) { return 0; }
static long devfs_write(struct vfs_file_t *file_ptr, char *buf, int64_t count, long *position) { return 0; }
static long devfs_lseek(struct vfs_file_t *file_ptr, long offset, long origin) { return 0; }
static long devfs_ioctl(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr, uint64_t cmd, uint64_t arg) { return 0; }

static long devfs_readdir(struct vfs_file_t *file_ptr, void *dirent, vfs_filldir_t filler)
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
    uint32_t dentry_type;
    if (target_dent->dir_inode->attribute & VFS_IF_DIR)
        dentry_type = VFS_IF_DIR;
    else
        dentry_type = VFS_IF_DEVICE;

    return filler(dirent, file_ptr->position - 1, name, target_dent->name_length, dentry_type, file_ptr->position - 1);
failed:;
    return 0;
}

struct vfs_file_operations_t devfs_file_ops =
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
    return 0;
}

static struct vfs_dir_entry_t *devfs_lookup(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dest_dEntry)
{
    /*
        由于devfs是伪文件系统，其所有的搜索都依赖于dentry缓存。
        因此，不需要根据inode来搜索目标目录项。除非目录项不存在，否则不会调用这个函数。
        当本函数调用的时候，也就意味着devfs中没有这个文件/文件夹。
        综上，本函数直接返回NULL即可
    */
    return NULL;
}
/**
 * @brief 在devfs中创建文件夹(作用是完善子文件夹的inode信息)
 *
 * @param inode 父目录的inode
 * @param dEntry 目标dentry
 * @param mode 创建模式
 * @return long 错误码
 */
static long devfs_mkdir(struct vfs_index_node_t *inode, struct vfs_dir_entry_t *dEntry, int mode)
{
    dEntry->dir_inode = vfs_alloc_inode();
    dEntry->dir_inode->file_ops = &devfs_file_ops;
    dEntry->dir_inode->inode_ops = &devfs_inode_ops;
    dEntry->dir_ops = &devfs_dentry_ops;
    // todo: 增加private inode info
    dEntry->dir_inode->private_inode_info = NULL;
    dEntry->dir_inode->sb = &devfs_sb;
    dEntry->dir_inode->attribute = VFS_IF_DIR;
    return 0;
}

static long devfs_rmdir(struct vfs_index_node_t *inode, struct vfs_dir_entry_t *dEntry) { return 0; }
static long devfs_rename(struct vfs_index_node_t *old_inode, struct vfs_dir_entry_t *old_dEntry, struct vfs_index_node_t *new_inode, struct vfs_dir_entry_t *new_dEntry) { return 0; }
static long devfs_getAttr(struct vfs_dir_entry_t *dEntry, uint64_t *attr) { return 0; }
static long devfs_setAttr(struct vfs_dir_entry_t *dEntry, uint64_t *attr) { return 0; }
struct vfs_inode_operations_t devfs_inode_ops = {
    .create = &devfs_create,
    .lookup = &devfs_lookup,
    .mkdir = &devfs_mkdir,
    .rmdir = &devfs_rmdir,
    .rename = &devfs_rename,
    .getAttr = &devfs_getAttr,
    .setAttr = &devfs_setAttr,
};

struct vfs_filesystem_type_t devfs_fs_type =
    {
        .name = "DEVFS",
        .fs_flags = 0,
        .read_superblock = devfs_read_superblock,
        .next = NULL,
};

static __always_inline void __devfs_init_root_inode()
{
    devfs_root_dentry->dir_inode = vfs_alloc_inode();
    devfs_root_dentry->dir_inode->file_ops = &devfs_file_ops;
    devfs_root_dentry->dir_inode->inode_ops = &devfs_inode_ops;

    // todo: 增加private inode info
    devfs_root_dentry->dir_inode->private_inode_info = NULL;
    devfs_root_dentry->dir_inode->sb = &devfs_sb;
    devfs_root_dentry->dir_inode->attribute = VFS_IF_DIR;
}
/**
 * @brief 初始化devfs的根dentry
 */
static __always_inline void __devfs_init_root_dentry()
{
    devfs_root_dentry = vfs_alloc_dentry(0);
    devfs_root_dentry->dir_ops = &devfs_dentry_ops;

    __devfs_init_root_inode();
}

/**
 * @brief 在devfs中注册设备
 *
 * @param device_type 设备主类型
 * @param sub_type 设备子类型
 * @param file_ops 设备的文件操作接口
 * @param ret_private_inode_info_ptr 返回的指向inode私有信息结构体的指针
 * @return int 错误码
 */
int devfs_register_device(uint16_t device_type, uint16_t sub_type, struct vfs_file_operations_t *file_ops, struct devfs_private_inode_info_t **ret_private_inode_info_ptr)
{
    spin_lock(&devfs_global_lock);
    int retval = 0;
    // 申请private info结构体
    struct devfs_private_inode_info_t *private_info = (struct devfs_private_inode_info_t *)kzalloc(sizeof(struct devfs_private_inode_info_t), 0);
    private_info->f_ops = file_ops;
    private_info->type = device_type;
    private_info->sub_type = sub_type;
    private_info->uuid = __devfs_get_uuid();

    struct vfs_dir_entry_t *dentry = NULL; // 该指针由对应类型设备的注册函数设置

    switch (device_type)
    {
    case DEV_TYPE_CHAR:
        retval = __devfs_chardev_register(private_info, &dentry);
        break;

    default:
        kerror("Unsupported device type [ %d ].", device_type);
        retval = -ENOTSUP;
        goto failed;
        break;
    }
    if (ret_private_inode_info_ptr != NULL)
        *ret_private_inode_info_ptr = private_info;

    spin_unlock(&devfs_global_lock);
    return retval;
failed:;
    kfree(private_info);
    spin_unlock(&devfs_global_lock);
    return retval;
}

/**
 * @brief 卸载设备
 *
 * @param private_inode_info 待卸载的设备的inode私有信息
 * @param put_private_info 设备被卸载后，执行的函数
 * @return int 错误码
 */
int devfs_unregister_device(struct devfs_private_inode_info_t *private_inode_info)
{
    int retval = 0;
    spin_lock(&devfs_global_lock);
    struct vfs_dir_entry_t *base_dentry = NULL;
    struct vfs_dir_entry_t *target_dentry = NULL;

    // 找到父目录的dentry
    {

        char base_path[64] = {0};
        switch (private_inode_info->type)
        {
        case DEV_TYPE_CHAR:
            strcpy(base_path, "/dev/char");
            break;
        default:
            retval = -ENOTSUP;
            goto out;
            break;
        }

        base_dentry = vfs_path_walk(base_path, 0);
        // bug
        if (unlikely(base_dentry == NULL))
        {
            BUG_ON(1);
            retval = -ENODEV;
            goto out;
        }
    }

    // 遍历子目录，寻找拥有指定inode的dentry（暂时不支持一个inode对应多个dentry的情况）
    // todo: 支持链接文件的卸载
    struct List *tmp_list = NULL, *target_list = NULL;
    list_for_each_safe(target_list, tmp_list, &base_dentry->subdirs_list)
    {
        target_dentry = list_entry(target_list, struct vfs_dir_entry_t, child_node_list);
        if (target_dentry->dir_inode == private_inode_info->inode)
        {
            spin_lock(&target_dentry->lockref.lock);
            retval = vfs_dentry_put(target_dentry);
            if (retval < 0)
            {
                kerror("Error when try to unregister device");
                spin_unlock(&target_dentry->lockref.lock);
            }
            else if (retval == 0) // 该设备的所有dentry均被卸载完成，不必继续迭代
                break;
        }
    }
    retval = 0;
out:;
    spin_unlock(&devfs_global_lock);
    return retval;
}

/**
 * @brief 初始化devfs
 *
 */
void devfs_init()
{
    __devfs_init_root_dentry();
    vfs_register_filesystem(&devfs_fs_type);
    spin_init(&devfs_global_lock);
    vfs_mount_fs(__devfs_mount_path, "DEVFS", NULL);

    __devfs_chardev_init();
}