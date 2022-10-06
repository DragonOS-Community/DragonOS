#include <common/unistd.h>

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