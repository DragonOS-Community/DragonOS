#include <errno.h>
#include <fcntl.h>
#include <libsystem/syscall.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

/**
 * @brief 关闭文件接口
 *
 * @param fd 文件描述符
 * @return int
 */
int close(int fd)
{
    return syscall_invoke(SYS_CLOSE, fd, 0, 0, 0, 0, 0);
}

/**
 * @brief 从文件读取数据的接口
 *
 * @param fd 文件描述符
 * @param buf 缓冲区
 * @param count 待读取数据的字节数
 * @return ssize_t 成功读取的字节数
 */
ssize_t read(int fd, void *buf, size_t count)
{
    return (ssize_t)syscall_invoke(SYS_READ, fd, (uint64_t)buf, count, 0, 0, 0);
}

/**
 * @brief 向文件写入数据的接口
 *
 * @param fd 文件描述符
 * @param buf 缓冲区
 * @param count 待写入数据的字节数
 * @return ssize_t 成功写入的字节数
 */
ssize_t write(int fd, void const *buf, size_t count)
{
    return (ssize_t)syscall_invoke(SYS_WRITE, fd, (uint64_t)buf, count, 0, 0, 0);
}

/**
 * @brief 调整文件的访问位置
 *
 * @param fd 文件描述符号
 * @param offset 偏移量
 * @param whence 调整模式
 * @return uint64_t 调整结束后的文件访问位置
 */
off_t lseek(int fd, off_t offset, int whence)
{
    return (off_t)syscall_invoke(SYS_LSEEK, fd, offset, whence, 0, 0, 0);
}

/**
 * @brief fork当前进程
 *
 * @return pid_t
 */
pid_t fork(void)
{
    return (pid_t)syscall_invoke(SYS_FORK, 0, 0, 0, 0, 0, 0);
}
/**
 * @brief 调用匿名管道
 *
 * @return int 如果失败返回负数
 */
int pipe(int fd[2])
{
    return (int)syscall_invoke(SYS_PIPE, fd, 0, 0, 0, 0, 0);
}
/**
 * @brief 调用带参数的匿名管道
 *
 * @return int 如果失败返回负数
 */
int pipe2(int fd[2], int flags)
{
    return (int)syscall_invoke(SYS_PIPE, fd, flags, 0, 0, 0, 0);
}
/**
 * @brief fork当前进程，但是与父进程共享VM、flags、fd
 *
 * @return pid_t
 */
pid_t vfork(void)
{
    return (pid_t)syscall_invoke(SYS_VFORK, 0, 0, 0, 0, 0, 0);
}

/**
 * @brief 将堆内存调整为end_brk
 *
 * @param end_brk 新的堆区域的结束地址
 * end_brk=-1  ===> 返回堆区域的起始地址
 * end_brk=-2  ===> 返回堆区域的结束地址
 * @return uint64_t 错误码
 *
 */
uint64_t brk(uint64_t end_brk)
{
    uint64_t x = (uint64_t)syscall_invoke(SYS_BRK, (uint64_t)end_brk, 0, 0, 0, 0, 0);
    if (x < end_brk)
    {
        errno = -ENOMEM;
        return -1;
    }
    return 0;
}

/**
 * @brief 将堆内存空间加上offset（注意，该系统调用只应在普通进程中调用，而不能是内核线程）
 *
 * @param increment offset偏移量
 * @return uint64_t the previous program break
 */
void *sbrk(int64_t increment)
{
    void *retval = (void *)syscall_invoke(SYS_SBRK, (uint64_t)increment, 0, 0, 0, 0, 0);
    if (retval == (void *)-ENOMEM)
        return (void *)(-1);
    else
    {
        errno = 0;
        return (void *)retval;
    }
}

/**
 * @brief 切换当前工作目录
 *
 * @param dest_path 目标目录
 * @return int64_t 成功：0,失败：负值（错误码）
 */
int64_t chdir(char *dest_path)
{
    if (dest_path == NULL)
    {
        errno = -EFAULT;
        return -1;
    }
    else
    {
        return syscall_invoke(SYS_CHDIR, (uint64_t)dest_path, 0, 0, 0, 0, 0);
    }
}

/**
 * @brief 执行新的程序
 *
 * @param path 文件路径
 * @param argv 参数列表
 * @return int
 */
int execv(const char *path, char *const argv[])
{
    if (path == NULL)
    {
        errno = -ENOENT;
        return -1;
    }

    int retval = syscall_invoke(SYS_EXECVE, (uint64_t)path, (uint64_t)argv, 0, 0, 0, 0);
    if (retval != 0)
        return -1;
    else
        return 0;
}

/**
 * @brief 删除文件夹
 *
 * @param path 绝对路径
 * @return int 错误码
 */
int rmdir(const char *path)
{
    return syscall_invoke(SYS_UNLINK_AT, 0, (uint64_t)path, AT_REMOVEDIR, 0, 0, 0);
}

/**
 * @brief 删除文件
 *
 * @param path 绝对路径
 * @return int
 */
int rm(const char *path)
{
    return syscall_invoke(SYS_UNLINK_AT, 0, (uint64_t)path, 0, 0, 0, 0);
}

/**
 * @brief  交换n字节
 *  @param src  源地址
 *  @param dest  目的地址
 * @param nbytes  交换字节数
 */
void swab(void *restrict src, void *restrict dest, ssize_t nbytes)
{
    unsigned char buf[32];
    char *_src = src;
    char *_dest = dest;
    uint32_t transfer;
    for (; nbytes > 0; nbytes -= transfer)
    {
        transfer = (nbytes > 32) ? 32 : nbytes;
        memcpy(buf, _src, transfer);
        memcpy(_src, _dest, transfer);
        memcpy(_dest, buf, transfer);
        _src += transfer;
        _dest += transfer;
    }
}

/**
 * @brief 获取当前进程的pid（进程标识符）
 *
 * @return pid_t 当前进程的pid
 */
pid_t getpid(void)
{
    return syscall_invoke(SYS_GETPID, 0, 0, 0, 0, 0, 0);
}

int dup(int fd)
{
    return syscall_invoke(SYS_DUP, fd, 0, 0, 0, 0, 0);
}

int dup2(int ofd, int nfd)
{
    return syscall_invoke(SYS_DUP2, ofd, nfd, 0, 0, 0, 0);
}

char *getcwd(char *buf, size_t size)
{
    return syscall_invoke(SYS_GETCWD, buf, size, 0, 0, 0, 0);
}