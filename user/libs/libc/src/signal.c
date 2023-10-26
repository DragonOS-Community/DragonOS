#include <signal.h>
#include <printf.h>
#include <stddef.h>
#include <libsystem/syscall.h>

#pragma GCC push_options
#pragma GCC optimize("O0")
void __libc_sa_restorer()
{
    // 在这里发起sigreturn,请注意，由于内核需要读取到原来的do_signal时保存的栈帧，因此这里不能发生函数调用（会导致函数压栈），只能够这样来完成sigreturn
    __asm__ __volatile__("int $0x80   \n\t" ::"a"(SYS_RT_SIGRETURN) : "memory");
}
#pragma GCC pop_options

/**
 * @brief 设置信号处理动作（简单版本）
 *
 * @param signum
 * @param handler
 * @return int
 */
int signal(int signum, __sighandler_t handler)
{
    struct sigaction sa = {0};
    sa.sa_handler = handler;
    // 由于DragonOS必须由用户程序指定一个sa_restorer，因此这里设置为libc的sa_restorer
    sa.sa_restorer = &__libc_sa_restorer;
    // printf("handler address: %#018lx\n", handler);
    // printf("restorer address: %#018lx\n", &__libc_sa_restorer);
    sigaction(signum, &sa, NULL);
}

/**
 * @brief 设置信号处理动作
 *
 * @param signum 信号
 * @param act 处理动作（不可为NULL）
 * @param oldact 返回的旧的处理动作（若为NULL，则不返回）
 * @return int 错误码（遵循posix）
 */
int sigaction(int signum, const struct sigaction *act, struct sigaction *oldact)
{
    return syscall_invoke(SYS_SIGACTION, (uint64_t)signum, (uint64_t)act, (uint64_t)oldact, 0, 0, 0);
}

/**
 * @brief 向当前进程发送一个信号
 *
 * @param sig signal number
 * @return int 错误码
 */
int raise(int sig)
{
    return kill(getpid(), sig);
}

/**
 * @brief 
 *
 * @param pid 进程的标识符
 * @param sig signal number
 * @return int 错误码
 */
int kill(pid_t pid, int sig)
{
    syscall_invoke(SYS_KILL, pid, sig, 0, 0, 0, 0);
}