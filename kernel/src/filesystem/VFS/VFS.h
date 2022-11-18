/**
 * @file VFS.h
 * @author fslongjin (longjin@RinGoTek.cn)
 * @brief 虚拟文件系统
 * @version 0.1
 * @date 2022-04-20
 *
 * @copyright Copyright (c) 2022
 *
 */

#pragma once

#include <common/blk_types.h>
#include <common/fcntl.h>
#include <common/glib.h>
#include <common/lockref.h>
#include <common/user_namespace.h>
#include <mm/slab.h>

extern struct vfs_superblock_t *vfs_root_sb;

#define VFS_DPT_MBR 0 // MBR分区表
#define VFS_DPT_GPT 1 // GPT分区表

#define VFS_MAX_PATHLEN 1024

/**
 * @brief inode的属性
 *
 */
#define VFS_IF_FILE (1UL << 0)
#define VFS_IF_DIR (1UL << 1) // 文件夹
#define VFS_IF_DEVICE (1UL << 2)
#define VFS_IF_DEAD (1UL << 3) /* removed, but still open directory */

struct vfs_super_block_operations_t;
struct vfs_inode_operations_t;

struct vfs_index_node_t;
struct vfs_dir_entry_operations_t;

#define VFS_DF_MOUNTED (1 << 0)      // 当前dentry是一个挂载点
#define VFS_DF_CANNOT_MOUNT (1 << 1) // 当前dentry是一个挂载点
struct vfs_dir_entry_t
{
    char *name;
    int name_length;  // 名字的长度（不包含字符串末尾的'\0'）
    uint32_t d_flags; // dentry标志位
    struct List child_node_list;
    struct List subdirs_list;

    struct lockref lockref; // 该lockref包含了dentry的自旋锁以及引用计数
    struct vfs_index_node_t *dir_inode;
    struct vfs_dir_entry_t *parent;
    struct vfs_dir_entry_operations_t *dir_ops;
};

struct vfs_superblock_t
{
    struct vfs_dir_entry_t *root;
    struct vfs_super_block_operations_t *sb_ops;
    struct vfs_dir_entry_operations_t *dir_ops; // dentry's operations
    struct block_device *blk_device;
    void *private_sb_info;
};

/**
 * @brief inode结构体
 *
 */
struct vfs_index_node_t
{
    uint64_t file_size; // 文件大小
    uint64_t blocks;    // 占用的扇区数
    uint64_t attribute;
    struct lockref lockref; // 自旋锁与引用计数

    struct vfs_superblock_t *sb;
    struct vfs_file_operations_t *file_ops;
    struct vfs_inode_operations_t *inode_ops;

    void *private_inode_info;
};

/**
 * @brief 文件的mode
 *
 */
#define VFS_FILE_MODE_READ (1 << 0)
#define VFS_FILE_MODE_WRITE (1 << 1)
#define VFS_FILE_MODE_RW (VFS_FILE_MODE_READ | VFS_FILE_MODE_WRITE)

#define vfs_file_can_read(file) (((file)->mode) & VFS_FILE_MODE_READ)
#define vfs_file_can_write(file) (((file)->mode) & VFS_FILE_MODE_WRITE)
#define vfs_file_can_rw(file) ((((file)->mode) & VFS_FILE_MODE_RW) == VFS_FILE_MODE_RW)

/**
 * @brief 文件描述符
 *
 */
struct vfs_file_t
{
    long position;
    uint64_t mode;

    struct vfs_dir_entry_t *dEntry;
    struct vfs_file_operations_t *file_ops;
    void *private_data;
};

struct vfs_filesystem_type_t
{
    char *name;
    int fs_flags;
    struct vfs_superblock_t *(*read_superblock)(
        struct block_device *blk); // 解析文件系统引导扇区的函数，为文件系统创建超级块结构。
    struct vfs_filesystem_type_t *next;
};

struct vfs_super_block_operations_t
{
    void (*write_superblock)(struct vfs_superblock_t *sb); // 将超级块信息写入磁盘
    void (*put_superblock)(struct vfs_superblock_t *sb);
    void (*write_inode)(struct vfs_index_node_t *inode); // 将inode信息写入磁盘
};

/**
 * @brief 对vfs的inode的操作抽象
 *
 */
struct vfs_inode_operations_t
{
    /**
     * @brief 创建新的文件
     * @param parent_inode 父目录的inode结构体
     * @param dest_dEntry 新文件的dentry
     * @param mode 创建模式
     */
    long (*create)(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dest_dEntry, int mode);
    /**
     * @brief 在文件系统中查找指定的目录项
     * @param parent_inode 父目录项（在这个目录下查找）
     * @param dest_dEntry 构造的目标目录项的结构体（传入名称，然后更多的详细信息将在本函数中完成填写）
     *
     */
    struct vfs_dir_entry_t *(*lookup)(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dest_dEntry);
    /**
     * @brief 创建文件夹
     * @param inode 父目录的inode
     * @param dEntry 新的文件夹的dentry
     * @param mode 创建文件夹的mode
     */
    long (*mkdir)(struct vfs_index_node_t *inode, struct vfs_dir_entry_t *dEntry, int mode);
    long (*rmdir)(struct vfs_index_node_t *inode, struct vfs_dir_entry_t *dEntry);
    long (*rename)(struct vfs_index_node_t *old_inode, struct vfs_dir_entry_t *old_dEntry,
                   struct vfs_index_node_t *new_inode, struct vfs_dir_entry_t *new_dEntry);
    long (*getAttr)(struct vfs_dir_entry_t *dEntry, uint64_t *attr);
    long (*setAttr)(struct vfs_dir_entry_t *dEntry, uint64_t *attr);
    
    /**
     * @brief 取消inode和dentry之间的链接关系（删除文件）
     * 
     * @param inode 要被取消关联关系的目录项的【父目录项】
     * @param dentry 要被取消关联关系的子目录项
     */
    long (*unlink)(struct vfs_index_node_t * inode, struct vfs_dir_entry_t * dentry);
};

struct vfs_dir_entry_operations_t
{
    long (*compare)(struct vfs_dir_entry_t *parent_dEntry, char *source_filename, char *dest_filename);
    long (*hash)(struct vfs_dir_entry_t *dEntry, char *filename);
    long (*release)(struct vfs_dir_entry_t *dEntry);
    long (*iput)(struct vfs_dir_entry_t *dEntry, struct vfs_index_node_t *inode);
};

/**
 * @brief 填充dirent的函数指针的类型定义
 *
 */
typedef int (*vfs_filldir_t)(void *buf, ino_t d_ino, char *name, int namelen, unsigned char type, off_t offset);

struct vfs_file_operations_t
{
    long (*open)(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr);
    long (*close)(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr);
    long (*read)(struct vfs_file_t *file_ptr, char *buf, int64_t count, long *position);
    long (*write)(struct vfs_file_t *file_ptr, char *buf, int64_t count, long *position);
    long (*lseek)(struct vfs_file_t *file_ptr, long offset, long origin);
    long (*ioctl)(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr, uint64_t cmd, uint64_t arg);

    long (*readdir)(struct vfs_file_t *file_ptr, void *dirent, vfs_filldir_t filler); // 读取文件夹
};

/**
 * @brief 在VFS中注册文件系统
 *
 * @param fs 文件系统类型结构体
 * @return uint64_t
 */
uint64_t vfs_register_filesystem(struct vfs_filesystem_type_t *fs);
uint64_t vfs_unregister_filesystem(struct vfs_filesystem_type_t *fs);

/**
 * @brief 挂载文件系统
 *
 * @param path 要挂载到的路径
 * @param name 文件系统名
 * @param blk 块设备结构体
 * @return struct vfs_superblock_t* 挂载后，文件系统的超级块
 */
struct vfs_superblock_t *vfs_mount_fs(const char *path, char *name, struct block_device *blk);

/**
 * @brief 按照路径查找文件
 *
 * @param path 路径
 * @param flags 1：返回父目录项， 0：返回结果目录项
 * @return struct vfs_dir_entry_t* 目录项
 */
struct vfs_dir_entry_t *vfs_path_walk(const char *path, uint64_t flags);

/**
 * @brief 填充dentry
 *
 */
int vfs_fill_dirent(void *buf, ino_t d_ino, char *name, int namelen, unsigned char type, off_t offset);

/**
 * @brief 初始化vfs
 *
 * @return int 错误码
 */
int vfs_init();

/**
 * @brief 动态分配dentry以及路径字符串名称
 *
 * @param name_size 名称字符串大小（字节）(注意考虑字符串最后需要有一个‘\0’作为结尾)
 * @return struct vfs_dir_entry_t* 创建好的dentry
 */
struct vfs_dir_entry_t *vfs_alloc_dentry(const int name_size);

/**
 * @brief 分配inode并将引用计数初始化为1
 *
 * @return struct vfs_index_node_t * 分配得到的inode
 */
struct vfs_index_node_t *vfs_alloc_inode();

/**
 * @brief 打开文件
 *
 * @param filename 文件路径
 * @param flags 标志位
 * @return uint64_t 错误码
 */
uint64_t do_open(const char *filename, int flags);

/**
 * @brief 创建文件夹
 *
 * @param path 文件夹路径
 * @param mode 创建模式
 * @param from_userland 该创建请求是否来自用户态
 * @return int64_t 错误码
 */
int64_t vfs_mkdir(const char *path, mode_t mode, bool from_userland);

/**
 * @brief 删除文件夹
 *
 * @param path 文件夹路径
 * @param from_userland 请求是否来自用户态
 * @return int64_t 错误码
 */
int64_t vfs_rmdir(const char *path, bool from_userland);

/**
 * @brief 释放dentry，并视情况自动释放inode。 在调用该函数前，需要将dentry加锁。
 *
 * @param dentry 目标dentry
 *
 * @return 错误码
 *          注意，当dentry指向文件时，如果返回值为正数，则表示在释放了该dentry后，该dentry指向的inode的引用计数。
 */
int vfs_dentry_put(struct vfs_dir_entry_t *dentry);

int vfs_unlink(struct user_namespace *mnt_userns, struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dentry,
               struct vfs_index_node_t **delegated_inode);

int do_unlink_at(int dfd, const char *pathname, bool name);