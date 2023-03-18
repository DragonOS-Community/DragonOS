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
#include <DragonOS/stdint.h>
#include <mm/slab.h>

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
    long (*unlink)(struct vfs_index_node_t *inode, struct vfs_dir_entry_t *dentry);
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
 * @brief 初始化vfs
 *
 * @return int 错误码
 */
extern int vfs_init();
