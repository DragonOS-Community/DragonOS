#include <fcntl.h>
#include <libsystem/syscall.h>

/**
 * @brief 打开文件的接口
 *
 * @param path 文件路径
 * @param options 打开选项
 * @param ...
 * @return int 文件描述符
 */
int open(const char *path, int options, ...)
{
    return syscall_invoke(SYS_OPEN, (uint64_t)path, options, 0, 0, 0, 0, 0, 0);
}

/**
 * @brief ioctl的接口
 *
 * @param path 文件路径
 * @param options 打开选项
 * @param ...
 * @return int 文件描述符
 */
int ioctl(int fd, int cmd, ...)
{
    return syscall_invoke(SYS_IOCTL, fd, cmd, 0, 0, 0, 0, 0, 0);
}