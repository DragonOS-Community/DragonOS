#pragma once

#include "ptrace.h"
#include <DragonOS/signal.h>
#include <DragonOS/stdint.h>
#include <common/wait_queue.h>

// 进程最大可拥有的文件描述符数量
#define PROC_MAX_FD_NUM 16

// 进程的内核栈大小 32K
#define STACK_SIZE 32768

// 进程的运行状态
// 正在运行
#define PROC_RUNNING (1 << 0)
// 可被信号打断
#define PROC_INTERRUPTIBLE (1 << 1)
// 不可被信号打断
#define PROC_UNINTERRUPTIBLE (1 << 2)
// 挂起
#define PROC_ZOMBIE (1 << 3)
// 已停止
#define PROC_STOPPED (1 << 4)

// 内核代码段基地址
#define KERNEL_CS (0x08)
// 内核数据段基地址
#define KERNEL_DS (0x10)
// 用户代码段基地址
#define USER_CS (0x28)
// 用户数据段基地址
#define USER_DS (0x30)

// 进程初始化时的数据拷贝标志位
#define CLONE_FS (1UL << 0)            // 在进程间共享打开的文件
#define CLONE_SIGNAL (1UL << 1)        // 克隆时，与父进程共享信号结构体
#define CLONE_VM (1UL << 2)            // 在进程间共享虚拟内存空间
#define CLONE_SIGHAND (1UL << 3)       // 克隆时，与父进程共享信号处理结构体
#define CLONE_CLEAR_SIGHAND (1UL << 4) // 克隆时，将原本被设置为SIG_IGNORE的信号，设置回SIG_DEFAULT
#define CLONE_THREAD (1UL << 5)        // 拷贝线程
#define PCB_NAME_LEN 16

struct thread_struct
{
    // 内核层栈基指针
    ul rbp; // in tss rsp0
    // 内核层代码指针
    ul rip;
    // 内核层栈指针
    ul rsp;

    ul fs, gs;

    ul cr2;
    // 异常号
    ul trap_num;
    // 错误码
    ul err_code;
};

// ========= pcb->flags =========
// 进程标志位
#define PF_KTHREAD (1UL << 0)    // 内核线程
#define PF_NEED_SCHED (1UL << 1) // 进程需要被调度
#define PF_VFORK (1UL << 2)      // 标志进程是否由于vfork而存在资源共享
#define PF_KFORK (1UL << 3)    // 标志在内核态下调用fork（临时标记，do_fork()结束后会将其复位）
#define PF_NOFREEZE (1UL << 4) // 当前进程不能被冻结
#define PF_EXITING (1UL << 5)  // 进程正在退出
#define PF_WAKEKILL (1UL << 6) // 进程由于接收到终止信号唤醒
#define PF_SIGNALED (1UL << 7) // 进程由于接收到信号而退出
#define PF_NEED_MIGRATE (1UL << 8) // 进程需要迁移到其他的核心

/**
 * @brief 进程控制块
 *
 */
struct process_control_block
{
    // 进程的状态
    volatile uint64_t state;
    // 进程标志：进程、线程、内核线程
    uint64_t flags;
    int32_t preempt_count; // 持有的自旋锁的数量

    uint32_t cpu_id; // 当前进程在哪个CPU核心上运行
    // pcb的名字
    char name[PCB_NAME_LEN];

    // 内存空间分布结构体， 记录内存页表和程序段信息
    struct mm_struct *mm;

    // 进程切换时保存的状态信息
    struct thread_struct *thread;

    // pcb加入调度队列时，所使用的链表节点
    struct List list;

    // todo:给pcb中加一个spinlock_t成员
    // 进程自旋锁
    //  spinlock_t alloc_lock;

    // 地址空间范围
    // 用户空间： 0x0000 0000 0000 0000 ~ 0x0000 7fff ffff ffff
    // 内核空间： 0xffff 8000 0000 0000 ~ 0xffff ffff ffff ffff
    uint64_t addr_limit;

    long pid;
    long priority;           // 优先级
    int64_t virtual_runtime; // 虚拟运行时间
    int64_t rt_time_slice;   // 由实时调度器管理的时间片

    // 进程拥有的文件描述符的指针数组(由Rust进行管理)
    void * fds;

    // 链表中的下一个pcb
    struct process_control_block *prev_pcb, *next_pcb;
    // 父进程的pcb
    struct process_control_block *parent_pcb;

    int32_t exit_code;                      // 进程退出时的返回码
    uint32_t policy;                        // 进程调度策略标志位
    wait_queue_node_t wait_child_proc_exit; // 子进程退出等待队列

    /* PF_kTHREAD  | PF_IO_WORKER 的进程，worker_private不为NULL*/
    void *worker_private;

    // ==== 信号处理相关 =====
    struct signal_struct *signal;
    struct sighand_struct *sighand;
    // 一个bitmap，表示当前进程被禁用的信号
    sigset_t sig_blocked;
    // 正在等待的信号的标志位，表示某个信号正在等待处理
    struct sigpending sig_pending;

    // 如果当前进程等待被迁移到另一个cpu核心上（也就是flags中的PF_NEED_MIGRATE被置位），
    // 该字段存储要被迁移到的目标处理器核心号
    uint32_t migrate_to;
    void* fp_state;//Fpstate 用于用户态切换到内核态时保存浮点寄存器里面的值
};

// 将进程的pcb和内核栈融合到一起,8字节对齐
union proc_union {
    struct process_control_block pcb;
    ul stack[STACK_SIZE / sizeof(ul)];
} __attribute__((aligned(8)));

struct tss_struct
{
    unsigned int reserved0;
    ul rsp0;
    ul rsp1;
    ul rsp2;
    ul reserved1;
    ul ist1;
    ul ist2;
    ul ist3;
    ul ist4;
    ul ist5;
    ul ist6;
    ul ist7;
    ul reserved2;
    unsigned short reserved3;
    // io位图基地址
    unsigned short io_map_base_addr;
} __attribute__((packed)); // 使用packed表明是紧凑结构，编译器不会对成员变量进行字节对齐。