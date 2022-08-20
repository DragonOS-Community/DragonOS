#include "stat.h"
#include <libsystem/syscall.h>

int mkdir(const char *path, mode_t mode)
{
    return syscall_invoke(SYS_MKDIR, (uint64_t)path, (uint64_t)mode, 0, 0, 0, 0, 0, 0);
}

/**
 * @brief 获取系统的内存信息
 *
 * @param stat 传入的内存信息结构体
 * @return int 错误码
 */
int mstat(struct mstat_t *stat)
{
    return syscall_invoke(SYS_MSTAT, (uint64_t)stat, 0, 0, 0, 0, 0, 0, 0);
}

int pipe(int *fd)
{
    return syscall_invoke(SYS_PIPE, (uint64_t)fd, 0, 0,0,0,0,0,0);
}