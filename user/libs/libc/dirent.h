#pragma once
#include <libc/sys/types.h>


/**
 * @brief inode的属性（copy from vfs.h）
 *
 */
#define VFS_IF_FILE (1UL << 0)
#define VFS_IF_DIR (1UL << 1)
#define VFS_IF_DEVICE (1UL << 2)

#define DIR_BUF_SIZE 256
/**
 * @brief 文件夹结构体
 *
 */
struct DIR
{
    int fd;
    int buf_pos;
    int buf_len;
    char buf[DIR_BUF_SIZE];

    // todo: 加一个指向dirent结构体的指针
};

struct dirent
{
    ino_t d_ino;    // 文件序列号
    off_t d_off;    // dir偏移量
    unsigned short d_reclen;    // 目录下的记录数
    unsigned char d_type;   // entry的类型
    char d_name[];   // 文件entry的名字(是一个零长数组)
};

/**
 * @brief 打开文件夹
 *
 * @param dirname
 * @return DIR*
 */
struct DIR *opendir(const char *dirname);

/**
 * @brief 关闭文件夹
 *
 * @param dirp DIR结构体指针
 * @return int 成功：0， 失败：-1
+--------+--------------------------------+
| errno  |              描述               |
+--------+--------------------------------+
|   0    |              成功               |
| -EBADF | 当前dirp不指向一个打开了的目录      |
| -EINTR |     函数执行期间被信号打断         |
+--------+--------------------------------+
 */
int closedir(struct DIR *dirp);

/**
 * @brief 从目录中读取数据
 * 
 * @param dir 
 * @return struct dirent* 
 */
struct dirent* readdir(struct DIR* dir);