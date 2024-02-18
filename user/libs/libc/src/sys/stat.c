#include <sys/stat.h>
#include <libsystem/syscall.h>
#include <unistd.h>
#include <fcntl.h>
#include <string.h>
#include <stddef.h>
#include <stdlib.h>

int mkdir(const char *path, mode_t mode)
{
    return syscall_invoke(SYS_MKDIR, (uint64_t)path, (uint64_t)mode, 0, 0, 0, 0);
}

/**
 * @brief 获取系统的内存信息
 *
 * @param stat 传入的内存信息结构体
 * @return int 错误码
 */
int mstat(struct mstat_t *stat)
{
    char *buf = (char *)malloc(128);
    memset(buf, 0, 128);
    int fd = open("/proc/meminfo", O_RDONLY);
    if (fd <= 0)
    {
        printf("ERROR: Cannot open file: /proc/meminfo, fd=%d\n", fd);
        return -1;
    }
    read(fd, buf, 127);
    close(fd);
    char *str = strtok(buf, "\n\t");
    char *value = (char *)malloc(strlen(str) - 3);
    int count = 0;
    while (str != NULL)
    {
        // printf("%d: %s\n", count, str);
        switch (count)
        {
        case 1:
            strncpy(value, str, strlen(str) - 3);
            stat->total = atoi(value);
            break;
        case 3:
            strncpy(value, str, strlen(str) - 3);
            stat->free = atoi(value);
            break;
        default:
            break;
        }
        str = strtok(NULL, "\n\t");
        count++;
    }
    stat->used = stat->total - stat->free;

    free(buf);
    free(value);
    return 0;
}
