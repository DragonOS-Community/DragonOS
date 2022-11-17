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
        int32_t code;                                                                                                  \
        int32_t si_errno;                                                                                              \
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
 *
 */
struct sigpending
{
    sigset_t signal;
};