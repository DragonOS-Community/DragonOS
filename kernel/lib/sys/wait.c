#include <syscall/syscall_num.h>
#include <syscall/syscall.h>

/**
 * @brief 等待指定pid的子进程退出
 *
 * @param pid 子进程的pid
 * @param stat_loc 返回的子进程结束状态
 * @param options 额外的控制选项
 * @return pid_t
 */
pid_t waitpid(pid_t pid, int *stat_loc, int options)
{
    return (pid_t)enter_syscall_int(SYS_WAIT4, (uint64_t)pid, (uint64_t)stat_loc, options, 0, 0, 0, 0, 0);
}