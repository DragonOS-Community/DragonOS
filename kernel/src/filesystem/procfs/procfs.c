#include "procfs.h"

struct vfs_super_block_operations_t procfs_sb_ops;
struct vfs_dir_entry_operations_t procfs_dentry_ops;
struct vfs_file_operations_t procfs_file_ops;
struct vfs_inode_operations_t procfs_inode_ops;

struct vfs_superblock_t procfs_sb = {0};
struct vfs_dir_entry_t *procfs_root_dentry; // 根结点的dentry
static spinlock_t procfs_global_lock;       // procfs的全局锁
const char __procfs_mount_path[] = "/proc"; // 挂在路径

/**
 * @brief 创建procfs的super block
 *
 * @param blk 未使用（procfs为伪文件系统，不需要物理设备）
 * @return struct vfs_superblock_t*
 */
struct vfs_superblock_t *procfs_read_superblock(struct block_device *blk)
{
    procfs_sb.blk_device = NULL;
    procfs_sb.root = procfs_root_dentry;
    procfs_sb.sb_ops = &procfs_sb_ops;
    procfs_sb.dir_ops = &procfs_dentry_ops;
    procfs_sb.private_sb_info = NULL;
    kdebug("procfs read superblock done");
    return &procfs_sb;
}

static void procfs_write_superblock(struct vfs_superblock_t *sb) { return; }
static void procfs_put_superblock(struct vfs_superblock_t *sb) { return; }
static void procfs_write_inode(struct vfs_index_node_t *inode) { return; }
struct vfs_super_block_operations_t procfs_sb_ops =
    {
        .write_superblock = &procfs_write_superblock,
        .put_superblock = &procfs_put_superblock,
        .write_inode = &procfs_write_inode,
};

static long procfs_compare(struct vfs_dir_entry_t *parent_dEntry, char *source_filename, char *dest_filename) { return 0; }
static long procfs_hash(struct vfs_dir_entry_t *dEntry, char *filename) { return 0; }
static long procfs_release(struct vfs_dir_entry_t *dEntry) { return 0; }
static long procfs_iput(struct vfs_dir_entry_t *dEntry, struct vfs_index_node_t *inode) { return 0; }
struct vfs_dir_entry_operations_t procfs_dentry_ops =
    {
        .compare = &procfs_compare,
        .hash = &procfs_hash,
        .release = &procfs_release,
        .iput = &procfs_iput,
};

static long procfs_open(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr) 
{ 
    struct procfs_inode_info_t *finode = inode->private_inode_info;
    struct proc_data *fdata = kzalloc(sizeof(struct proc_data),0);
    struct process_control_block *pcb_t = process_get_pcb(finode->pid);

    //判断文件类型
    int mode = finode->type;
    fdata->rbuffer = kzalloc(1024,0);
    int len = 0; 
    switch (mode)
    {
    case 1:
        strcpy(fdata->rbuffer, "Name:"); len+=5;
        strcpy((fdata->rbuffer)+len, pcb_t->name); len+=strlen(pcb_t->name);
        strcpy((fdata->rbuffer)+len, "\n\r"); len++;
        strcpy((fdata->rbuffer)+len, "state:"); len+=6;
        strcpy((fdata->rbuffer)+len, ltoa(pcb_t->state)); len+=strlen(ltoa(pcb_t->state));
        strcpy((fdata->rbuffer)+len, "\n\r"); len++;
        strcpy((fdata->rbuffer)+len, "pid:"); len+=4;
        strcpy((fdata->rbuffer)+len, ltoa(pcb_t->pid)); len+=strlen(ltoa(pcb_t->pid));
        strcpy((fdata->rbuffer)+len, "\n\r"); len++;
        strcpy((fdata->rbuffer)+len, "Ppid:"); len+=5;
        strcpy((fdata->rbuffer)+len, ltoa(pcb_t->parent_pcb->pid)); len+=strlen(ltoa(pcb_t->parent_pcb->pid));
        strcpy((fdata->rbuffer)+len, "\n\r"); len++;
        strcpy((fdata->rbuffer)+len, "cpu_id:"); len+=7;
        strcpy((fdata->rbuffer)+len, ltoa(pcb_t->cpu_id)); len+=strlen(ltoa(pcb_t->cpu_id));
        strcpy((fdata->rbuffer)+len, "\n\r"); len++;
        strcpy((fdata->rbuffer)+len, "signal:"); len+=7;
        strcpy((fdata->rbuffer)+len, ltoa(pcb_t->signal)); len+=strlen(ltoa(pcb_t->signal));
        strcpy((fdata->rbuffer)+len, "\n\r"); len++;
        strcpy((fdata->rbuffer)+len, "priority:"); len+=9;
        strcpy((fdata->rbuffer)+len, ltoa(pcb_t->priority)); len+=strlen(ltoa(pcb_t->priority));
        strcpy((fdata->rbuffer)+len, "\n\r"); len++;

        fdata->readlen = len;
        inode->file_size = len;
        file_ptr->private_data = fdata;
        break;
    
    default:
        break;
    }
    
    return 0; 
}
static long procfs_close(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr) { return 0; }
static long procfs_read(struct vfs_file_t *file_ptr, char *buf, int64_t count, long *position)
{
    // 获取私有信息
    struct proc_data *priv = (struct proc_data*)file_ptr->private_data;
    // kdebug("readlen:%ld",priv->readlen);
    if (!priv->rbuffer)
        return -EINVAL;

    return simple_procfs_read(buf, count, position, priv->rbuffer, priv->readlen);
}

/**
 * @brief 检查读取并将数据从内核拷贝到用户
 *
 * @param to: 要读取的用户空间缓冲区
 * @param count: 要读取的最大字节数
 * @param position: 缓冲区中的当前位置
 * @param from: 要读取的缓冲区
 * @param available: 缓冲区的大小
 *
 * @return long 读取字节数
 */
long simple_procfs_read(void *to, int64_t count, long *position, void *from, int64_t available)
{
    long pos = *position;
    // kdebug("pos:%ld",pos);
    // kdebug("count:%ld",count);
    // kdebug("available:%ld",available);
    int64_t ret;

    if (pos < 0)
        return -EINVAL;
    if (pos >= available || !count)
        return 0;
    if (count > available - pos)
        count = available - pos;
    // kdebug("count:%d",count);
    ret = copy_to_user(to, from + pos, count);
    // kdebug("to=%#018lx", to);
    // kdebug("from=%#018lx", from);
    // kdebug("ret:%ld",ret);
    // if (ret == count)
    //     return -EFAULT;
    // count -= ret;
    *position = pos + ret;
    return ret;
}

static long procfs_write(struct vfs_file_t *file_ptr, char *buf, int64_t count, long *position) { return 0; }
/**
 * @brief 调整文件的访问位置
 *
 * @param file_ptr 文件描述符号
 * @param offset 偏移量
 * @param whence 调整模式
 * @return uint64_t 调整结束后的文件访问位置
 */
static long procfs_lseek(struct vfs_file_t *file_ptr, long offset, long whence)
{
    struct vfs_index_node_t *inode = file_ptr->dEntry->dir_inode;

    long pos = 0;
    switch (whence)
    {
    case SEEK_SET: // 相对于文件头
        pos = offset;
        break;
    case SEEK_CUR: // 相对于当前位置
        pos = file_ptr->position + offset;
        break;
    case SEEK_END: // 相对于文件末尾
        pos = file_ptr->dEntry->dir_inode->file_size + offset;
        break;

    default:
        return -EINVAL;
        break;
    }

    if (pos < 0 || pos > file_ptr->dEntry->dir_inode->file_size)
        return -EOVERFLOW;
    file_ptr->position = pos;

    return pos;
}
static long procfs_ioctl(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr, uint64_t cmd, uint64_t arg) { return 0; }

/**
 * @brief 读取该目录下的目录项
 *
 * @param file_ptr 文件结构体的指针
 * @param dirent 返回的dirent
 * @param filler 填充dirent的函数
 *
 * @return long 错误码
 */
static long procfs_readdir(struct vfs_file_t *file_ptr, void *dirent, vfs_filldir_t filler)
{
    struct vfs_dir_entry_t *dentry = file_ptr->dEntry;
    struct List *list = &dentry->subdirs_list;
    // 先切换到position处
    for (int i = 0; i <= file_ptr->position; ++i)
    {
        list = list_next(list);
        if (list == &dentry->subdirs_list) // 找完了
            goto failed;
    }

    // 若存在目录项，则增加偏移量
    ++file_ptr->position;
    // 获取目标dentry（由于是子目录项，因此是child_node_list）
    struct vfs_dir_entry_t *target_dent = container_of(list, struct vfs_dir_entry_t, child_node_list);

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
struct vfs_file_operations_t procfs_file_ops =
    {
        .open = &procfs_open,
        .close = &procfs_close,
        .read = &procfs_read,
        .write = &procfs_write,
        .lseek = &procfs_lseek,
        .ioctl = &procfs_ioctl,
        .readdir = &procfs_readdir,
};

/**
 * @brief 在procfs中创建文件
 *
 * @param parent_inode 父目录的inode
 * @param dest_dEntry 目标dentry
 * @param mode 创建模式
 * @return long 错误码
 */
static long procfs_create(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dest_dEntry, int mode)
{
    int64_t retval = 0;

    //检验名称和法性
    retval = check_name_available(dest_dEntry->name, dest_dEntry->name_length, 0);
    if (retval != 0)
        return retval;
    if (dest_dEntry->dir_inode != NULL)
        return -EEXIST;

    struct vfs_index_node_t *inode = vfs_alloc_inode();
    dest_dEntry->dir_inode = inode;
    dest_dEntry->dir_ops = &procfs_dentry_ops;

    //结点信息初始化
    struct procfs_inode_info_t *finode = (struct procfs_inode_info_t *)kzalloc(sizeof(struct procfs_inode_info_t), 0);
    inode->attribute = VFS_IF_FILE;
    inode->file_ops = &procfs_file_ops;
    inode->file_size = 0;
    inode->sb = parent_inode->sb;
    inode->inode_ops = &procfs_inode_ops;
    inode->private_inode_info = (void *)finode;
    inode->blocks = 0;

    //私有信息初始

    return 0;
}
static struct vfs_dir_entry_t *procfs_lookup(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dest_dEntry) { return NULL; }

/**
 * @brief 在procfs中创建文件夹(作用是完善子文件夹的inode信息)
 *
 * @param inode 父目录的inode
 * @param dEntry 目标dentry
 * @param mode 创建模式
 * @return long 错误码
 */
static long procfs_mkdir(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dEntry, int mode)
{
    int64_t retval = 0;

    //检验名称和法性
    retval = check_name_available(dEntry->name, dEntry->name_length, 0);
    if (retval != 0)
        return retval;

    struct vfs_index_node_t *inode = vfs_alloc_inode();
    dEntry->dir_inode = inode;
    dEntry->dir_ops = &procfs_dentry_ops;

    //结点信息初始化
    struct procfs_inode_info_t *finode = (struct procfs_inode_info_t *)kzalloc(sizeof(struct procfs_inode_info_t), 0);
    inode->attribute = VFS_IF_DIR;
    inode->file_ops = &procfs_file_ops;
    inode->file_size = 0;
    inode->sb = parent_inode->sb;
    inode->inode_ops = &procfs_inode_ops;
    inode->private_inode_info = (void *)finode;
    inode->blocks = 0;

    return 0;
}
struct vfs_inode_operations_t procfs_inode_ops = {
    .create = &procfs_create,
    .lookup = &procfs_lookup,
    .mkdir = &procfs_mkdir,
};

struct vfs_filesystem_type_t procfs_fs_type =
    {
        .name = "procfs",
        .fs_flags = 0,
        .read_superblock = procfs_read_superblock,
        .next = NULL,
};

static __always_inline void __procfs_init_root_inode()
{
    procfs_root_dentry->dir_inode = vfs_alloc_inode();
    procfs_root_dentry->dir_inode->file_ops = &procfs_file_ops;
    procfs_root_dentry->dir_inode->inode_ops = &procfs_inode_ops;

    procfs_root_dentry->dir_inode->private_inode_info = NULL;
    procfs_root_dentry->dir_inode->sb = &procfs_sb;
    procfs_root_dentry->dir_inode->attribute = VFS_IF_DIR;
}
/**
 * @brief 初始化procfs的根dentry
 */
static __always_inline void __procfs_init_root_dentry()
{
    procfs_root_dentry = vfs_alloc_dentry(0);
    procfs_root_dentry->dir_ops = &procfs_dentry_ops;

    __procfs_init_root_inode();
}

/**
 * @brief 创建进程对应文件夹
 *
 * @param pid 进程号
 * @return int64_t 错误码
 */
int64_t mk_proc_dir(long pid)
{
    int retval = 0;
    
    //创建文件夹
    char tmp[70]={0};
    int len = strlen(ltoa(pid));
    // kdebug("len:%d",len);
    strcpy(tmp, "/proc/");
    strcpy(tmp+6,ltoa(pid));
    // kdebug("tmp:%s",tmp);
    retval = vfs_mkdir(tmp, 0, false);
    
    //创建各相关文件
    strcpy(tmp+6+len,"/status");
    // kdebug("tmp:%s",tmp);
    retval = proc_create_file(tmp,1,pid);
    
    return retval;
}

/**
 * @brief 创建文件
 *
 * @param path 文件夹路径
 * @param mode 创建模式
 * @param procfs_ops 文件特定操作
 * @return int64_t 错误码
 */
int64_t proc_create_file(const char *path, mode_t mode, long pid)
{
    long pathlen = strnlen(path, PAGE_4K_SIZE - 1);
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

    char *buf = (char *)kzalloc(last_slash + 2, 0);

    // 拷贝字符串（不包含要被创建的部分）
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

    //创建文件
    struct vfs_dir_entry_t *dentry = vfs_alloc_dentry(pathlen - last_slash);
    if (path[pathlen - 1] == '/')
        dentry->name_length = pathlen - last_slash - 2;
    else
        dentry->name_length = pathlen - last_slash - 1;

    strncpy(dentry->name, path + last_slash + 1, dentry->name_length);
    dentry->parent = parent_dir;

    // 对父目录项加锁
    spin_lock(&parent_dir->lockref.lock);
    spin_lock(&parent_dir->dir_inode->lockref.lock);
    // 创建子目录项
    int64_t retval = 0;
    struct vfs_index_node_t *parent_inode = parent_dir->dir_inode;

    //检验名称和法性
    retval = check_name_available(dentry->name, dentry->name_length, 0);
    if (retval != 0)
        return retval;
    if (dentry->dir_inode != NULL)
        return -EEXIST;

    struct vfs_index_node_t *inode = vfs_alloc_inode();
    dentry->dir_inode = inode;
    dentry->dir_ops = &procfs_dentry_ops;

    //结点信息初始化
    struct procfs_inode_info_t *finode = (struct procfs_inode_info_t *)kzalloc(sizeof(struct procfs_inode_info_t), 0);
    finode->pid = pid;
    kdebug("pid:%d",finode->pid);
    finode->type = mode;

    inode->attribute = VFS_IF_FILE;
    inode->file_ops = &procfs_file_ops;
    inode->file_size = 0;
    inode->sb = parent_inode->sb;
    inode->inode_ops = &procfs_inode_ops;
    inode->private_inode_info = (void *)finode;
    kdebug("finode:%#018lx",inode->private_inode_info);
    inode->blocks = 0;

    spin_unlock(&parent_dir->dir_inode->lockref.lock); // 解锁inode

    // ==== 将子目录项添加到链表 ====
    struct vfs_dir_entry_t *next_dentry = NULL;
    // 若list非空，则对前一个dentry加锁
    if (!list_empty(&parent_dir->subdirs_list))
    {
        next_dentry = list_entry(list_next(&parent_dir->subdirs_list), struct vfs_dir_entry_t, child_node_list);
        spin_lock(&next_dentry->lockref.lock);
    }
    list_add(&parent_dir->subdirs_list, &dentry->child_node_list);
    if (next_dentry != NULL)
        spin_unlock(&next_dentry->lockref.lock);

    // 新建文件结束，对父目录项解锁
    spin_unlock(&parent_dir->lockref.lock);

    return retval;
}

/**
 * @brief 初始化procfs
 *
 */
void procfs_init()
{
    __procfs_init_root_dentry();
    vfs_register_filesystem(&procfs_fs_type);
    spin_init(&procfs_global_lock);
    vfs_mount_fs(__procfs_mount_path, "procfs", NULL);
}
