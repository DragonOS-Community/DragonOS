#pragma once
#include <libc/src/include/unistd.h>

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

typedef void (*__sighandler_t)(int);

#define SIG_DFL ((__sighandler_t)0) /* Default action.  */
#define SIG_IGN ((__sighandler_t)1) /* Ignore signal.  */

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

typedef struct
{
    union {
        __SIGINFO;
        uint64_t padding[4]; // 让siginfo占用32字节大小
    };
} siginfo_t;

typedef struct
{
    uint64_t set;
} sigset_t;

struct sigaction
{
    // sa_handler和sa_sigaction二选1
    __sighandler_t sa_handler;
    void (*sa_sigaction)(int, siginfo_t *, void *);
    sigset_t sa_mask;
    uint64_t sa_flags;
    void (*sa_restorer)(void);
};

int sigaction(int signum, const struct sigaction *act, struct sigaction *oldact);
int signal(int signum, __sighandler_t handler);
int raise(int sig);
int kill(pid_t, int sig);