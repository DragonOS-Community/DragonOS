#include <dirent.h>
#include <unistd.h>
#include <stdio.h>
#include <fcntl.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <libsystem/syscall.h>

/**
 * @brief 打开文件夹
 *
 * @param dirname
 * @return DIR*
 */
struct DIR *opendir(const char *path)
{
    int fd = open(path, O_DIRECTORY);
    if (fd < 0) // 目录打开失败
    {
        printf("Failed to open dir\n");
        return NULL;
    }
    // printf("open dir: %s\n", path);

    // 分配DIR结构体
    struct DIR *dirp = (struct DIR *)malloc(sizeof(struct DIR));
    // printf("dirp = %#018lx", dirp);
    memset(dirp, 0, sizeof(struct DIR));
    dirp->fd = fd;
    dirp->buf_len = DIR_BUF_SIZE;
    dirp->buf_pos = 0;

    return dirp;
}

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
int closedir(struct DIR *dirp)
{
    int retval = close(dirp->fd);
    free(dirp);
    return retval;
}

int64_t getdents(int fd, struct dirent *dirent, long count)
{
    return syscall_invoke(SYS_GET_DENTS, fd, (uint64_t)dirent, count, 0, 0, 0);
}
/**
 * @brief 从目录中读取数据
 *
 * @param dir
 * @return struct dirent*
 */
struct dirent *readdir(struct DIR *dir)
{
    // printf("dir->buf = %#018lx\n", (dir->buf));
    memset((dir->buf), 0, DIR_BUF_SIZE);
    // printf("memeset_ok\n");
    int len = getdents(dir->fd, (struct dirent *)dir->buf, DIR_BUF_SIZE);
    // printf("len=%d\n", len);
    if (len > 0)
        return (struct dirent *)dir->buf;
    else
        return NULL;
}