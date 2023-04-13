#pragma once
#include <sys/types.h>

#if defined(__cplusplus) 
extern  "C"  { 
#endif

/*
 * This is a header for the common implementation of dirent
 * to fs on-disk file type conversion.  Although the fs on-disk
 * bits are specific to every file system, in practice, many
 * file systems use the exact same on-disk format to describe
 * the lower 3 file type bits that represent the 7 POSIX file
 * types.
 *
 * It is important to note that the definitions in this
 * header MUST NOT change. This would break both the
 * userspace ABI and the on-disk format of filesystems
 * using this code.
 *
 * All those file systems can use this generic code for the
 * conversions.
 */

/*
 * struct dirent file types
 * exposed to user via getdents(2), readdir(3)
 *
 * These match bits 12..15 of stat.st_mode
 * (ie "(i_mode >> 12) & 15").
 */

// 完整含义请见 http://www.gnu.org/software/libc/manual/html_node/Directory-Entries.html
#define S_DT_SHIFT	12
#define S_DT(mode)	(((mode) & S_IFMT) >> S_DT_SHIFT)
#define S_DT_MASK	(S_IFMT >> S_DT_SHIFT)

/* these are defined by POSIX and also present in glibc's dirent.h */
#define DT_UNKNOWN	0
// 命名管道，或者FIFO
#define DT_FIFO		1
// 字符设备
#define DT_CHR		2
// 目录
#define DT_DIR		4
// 块设备
#define DT_BLK		6
// 常规文件
#define DT_REG		8
// 符号链接
#define DT_LNK		10
// 是一个socket
#define DT_SOCK		12
// 这个是抄Linux的，还不知道含义
#define DT_WHT		14

#define DT_MAX		(S_DT_MASK + 1) /* 16 */

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

#if defined(__cplusplus) 
}  /* extern "C" */ 
#endif