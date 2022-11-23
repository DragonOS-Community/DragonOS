#pragma once
#include <DragonOS/refcount.h>
#include <common/atomic.h>
#include <common/list.h>
#include <common/spinlock.h>
#include <common/sys/types.h>
#include <common/wait_queue.h>

// 系统最大支持的信号数量
#define MAX_SIG_NUM 64

typedef void __signalfn_t(int);
typedef __signalfn_t *__sighandler_t;

typedef uint64_t sigset_t;

#define SIGHUP 1
#define SIGINT 2
#define SIGQUIT 3
#define SIGILL 4
#define SIGTRAP 5
#define SIGABRT 6
#define SIGIOT 6
#define SIGBUS 7
#define SIGFPE 8
#define SIGKILL 9
#define SIGUSR1 10
#define SIGSEGV 11
#define SIGUSR2 12
#define SIGPIPE 13
#define SIGALRM 14
#define SIGTERM 15
#define SIGSTKFLT 16
#define SIGCHLD 17
#define SIGCONT 18
#define SIGSTOP 19
#define SIGTSTP 20
#define SIGTTIN 21
#define SIGTTOU 22
#define SIGURG 23
#define SIGXCPU 24
#define SIGXFSZ 25
#define SIGVTALRM 26
#define SIGPROF 27
#define SIGWINCH 28
#define SIGIO 29
#define SIGPOLL SIGIO

#define SIGPWR 30
#define SIGSYS 31

/* These should not be considered constants from userland.  */
#define SIGRTMIN 32
#define SIGRTMAX MAX_SIG_NUM

// 注意，该结构体最大16字节
union __sifields {
    /* kill() */
    struct
    {
        pid_t _pid; /* 信号发送者的pid */
    } _kill;
};

// 注意，该结构体最大大小为32字节
#define __SIGINFO                                                                                                      \
    struct                                                                                                             \
    {                                                                                                                  \
        int32_t si_signo; /* signal number */                                                                          \
        int32_t si_code;                                                                                               \
        int32_t si_errno;                                                                                              \
        uint32_t reserved; /* 保留备用 */                                                                          \
        union __sifields _sifields;                                                                                    \
    }

struct siginfo
{
    union {
        __SIGINFO;
        uint64_t padding[4]; // 让siginfo占用32字节大小
    };
};

/**
 * @brief 信号处理结构体
 *
 */
struct sigaction
{
    // 信号处理函数的指针
    union {
        __sighandler_t _sa_handler;
        void (*_sa_sigaction)(int sig, struct siginfo *sinfo, void *);
    } _u;
    uint64_t sa_flags;
    sigset_t sa_mask;
    void (*sa_restorer)(void); // 暂时未实现
};

/**
 * 由于signal_struct总是和sighand_struct一起使用，并且信号处理的过程中必定会对sighand加锁，
 * 因此signal_struct不用加锁
 */
struct signal_struct
{
    atomic_t sig_cnt;
};

/**
 * @brief 信号处理结构体，位于pcb之中。
 *
 */
struct sighand_struct
{
    spinlock_t siglock;
    refcount_t count;
    wait_queue_head_t signal_fd_wqh;
    // 为每个信号注册的处理函数的结构体
    struct sigaction action[MAX_SIG_NUM];
};

/**
 * @brief 正在等待的信号的标志位
 */
struct sigpending
{
    sigset_t signal;
};