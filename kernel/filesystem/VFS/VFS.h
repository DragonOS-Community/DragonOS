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

#include <common/glib.h>

struct vfs_superblock_t *vfs_root_sb = NULL;

#define VFS_DPT_MBR 0 // MBR分区表
#define VFS_DPT_GPT 1 // GPT分区表

#define VFS_SUCCESS 0
#define VFS_E_FS_EXISTED 1   // 错误：文件系统已存在
#define VFS_E_FS_NOT_EXIST 2 // 错误：文件系统不存在

/**
 * @brief 目录项的属性
 *
 */
#define VFS_ATTR_FILE (1UL << 0)
#define VFS_ATTR_DIR (1UL << 1)

struct vfs_super_block_operations_t;
struct vfs_inode_operations_t;

struct vfs_index_node_t;
struct vfs_dir_entry_operations_t;

struct vfs_dir_entry_t
{
    char *name;
    int name_length;
    struct List child_node_list;
    struct List subdirs_list;

    struct vfs_index_node_t *dir_inode;
    struct vfs_dir_entry_t *parent;
    struct vfs_dir_entry_operations_t *dir_ops;
};

struct vfs_superblock_t
{
    struct vfs_dir_entry_t *root;
    struct vfs_super_block_operations_t *sb_ops;
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

    struct vfs_superblock_t *sb;
    struct vfs_file_operations_t *file_ops;
    struct vfs_inode_operations_t *inode_ops;

    void *private_inode_info;
};

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
    struct vfs_superblock_t *(*read_superblock)(void *DPTE, uint8_t DPT_type, void *buf, int8_t ahci_ctrl_num, int8_t ahci_port_num, int8_t part_num); // 解析文件系统引导扇区的函数，为文件系统创建超级块结构。其中DPTE为磁盘分区表entry（MBR、GPT不同）
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
    long (*create)(struct vfs_index_node_t *inode, struct vfs_dir_entry_t *dEntry, int mode);
    struct vfs_dir_entry_t *(*lookup)(struct vfs_index_node_t *parent_inode, struct vfs_dir_entry_t *dest_dEntry);
    long (*mkdir)(struct vfs_index_node_t *inode, struct vfs_dir_entry_t *dEntry, int mode);
    long (*rmdir)(struct vfs_index_node_t *inode, struct vfs_dir_entry_t *dEntry);
    long (*rename)(struct vfs_index_node_t *old_inode, struct vfs_dir_entry_t *old_dEntry, struct vfs_index_node_t *new_inode, struct vfs_dir_entry_t *new_dEntry);
    long (*getAttr)(struct vfs_dir_entry_t *dEntry, uint64_t *attr);
    long (*setAttr)(struct vfs_dir_entry_t *dEntry, uint64_t *attr);
};

struct vfs_dir_entry_operations_t
{
    long (*compare)(struct vfs_dir_entry_t *parent_dEntry, char *source_filename, char *dest_filename);
    long (*hash)(struct vfs_dir_entry_t *dEntry, char *filename);
    long (*release)(struct vfs_dir_entry_t *dEntry);
    long (*iput)(struct vfs_dir_entry_t *dEntry, struct vfs_index_node_t *inode);
};

struct vfs_file_operations_t
{
    long (*open)(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr);
    long (*close)(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr);
    long (*read)(struct vfs_file_t *file_ptr, char *buf, uint64_t count, long *position);
    long (*write)(struct vfs_file_t *file_ptr, char *buf, uint64_t count, long *position);
    long (*lseek)(struct vfs_file_t *file_ptr, long offset, long origin);
    long (*ioctl)(struct vfs_index_node_t *inode, struct vfs_file_t *file_ptr, uint64_t cmd, uint64_t arg);
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
 * @param name 文件系统名
 * @param DPTE 分区表entry
 * @param DPT_type 分区表类型
 * @param buf 文件系统的引导扇区
 * @return struct vfs_superblock_t*
 */
struct vfs_superblock_t *vfs_mount_fs(char *name, void *DPTE, uint8_t DPT_type, void *buf, int8_t ahci_ctrl_num, int8_t ahci_port_num, int8_t part_num);

/**
 * @brief 按照路径查找文件
 *
 * @param path 路径
 * @param flags 1：返回父目录项， 0：返回结果目录项
 * @return struct vfs_dir_entry_t* 目录项
 */
struct vfs_dir_entry_t *vfs_path_walk(char *path, uint64_t flags);