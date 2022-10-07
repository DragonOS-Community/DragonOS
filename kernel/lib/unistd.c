#include <common/unistd.h>
#include <common/glib.h>

/**
 * @brief fork当前进程
 *
 * @return pid_t
 */
pid_t fork(void)
{
    return (pid_t)enter_syscall_int(SYS_FORK, 0, 0, 0, 0, 0, 0, 0, 0);
}

/**
 * @brief vfork当前进程
 *
 * @return pid_t
 */
pid_t vfork(void)
{
    return (pid_t)enter_syscall_int(SYS_VFORK, 0, 0, 0, 0, 0, 0, 0, 0);
}

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