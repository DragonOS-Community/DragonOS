/**
 * @file process.h
 * @author longjin
 * @brief 进程
 * @date 2022-01-29
 *
 * @copyright Copyright (c) 2022
 *
 */

#pragma once
#include "ptrace.h"
#include <common/cpu.h>
#include <common/errno.h>
#include <common/glib.h>
#include <filesystem/vfs/VFS.h>
#include <mm/mm-types.h>
#include <syscall/syscall.h>

#include <asm/current.h>

#include "proc-types.h"

/**
 * @brief 任务状态段结构体
 *
 */

// 设置初始进程的tss
#define INITIAL_TSS                                                                                                   \
    {                                                                                                                 \
        .reserved0 = 0, .rsp0 = (ul)(initial_proc_union.stack + STACK_SIZE / sizeof(ul)),                             \
        .rsp1 = (ul)(initial_proc_union.stack + STACK_SIZE / sizeof(ul)),                                             \
        .rsp2 = (ul)(initial_proc_union.stack + STACK_SIZE / sizeof(ul)), .reserved1 = 0, .ist1 = 0xffff800000007c00, \
        .ist2 = 0xffff800000007c00, .ist3 = 0xffff800000007c00, .ist4 = 0xffff800000007c00,                           \
        .ist5 = 0xffff800000007c00, .ist6 = 0xffff800000007c00, .ist7 = 0xffff800000007c00, .reserved2 = 0,           \
        .reserved3 = 0, .io_map_base_addr = 0                                                                         \
    }

#define GET_CURRENT_PCB    \
    "movq %rsp, %rbx \n\t" \
    "andq $-32768, %rbx\n\t"

/**
 * @brief 切换进程上下文
 * 先把rbp和rax保存到栈中，然后将rsp和rip保存到prev的thread结构体中
 * 然后调用__switch_to切换栈，配置其他信息，最后恢复下一个进程的rax rbp。
 */

#define switch_to(prev, next)                                                                       \
    do                                                                                              \
    {                                                                                               \
        __asm__ __volatile__("pushq	%%rbp	\n\t"                                                     \
                             "pushq	%%rax	\n\t"                                                     \
                             "movq	%%rsp,	%0	\n\t"                                                  \
                             "movq	%2,	%%rsp	\n\t"                                                  \
                             "leaq	2f(%%rip),	%%rax	\n\t"                                           \
                             "movq	%%rax,	%1	\n\t"                                                  \
                             "pushq	%3		\n\t"                                                       \
                             "jmp	__switch_to	\n\t"                                                 \
                             "2:	\n\t"                                                              \
                             "popq	%%rax	\n\t"                                                      \
                             "popq	%%rbp	\n\t"                                                      \
                             : "=m"(prev->thread->rsp), "=m"(prev->thread->rip)                     \
                             : "m"(next->thread->rsp), "m"(next->thread->rip), "D"(prev), "S"(next) \
                             : "memory", "rax");                                                    \
    } while (0)

/**
 * @brief 初始化系统的第一个进程
 *
 */
void process_init();

/**
 * @brief 根据pid获取进程的pcb。存在对应的pcb时，返回对应的pcb的指针，否则返回NULL
 * 当进程管理模块拥有pcblist_lock之后，调用本函数之前，应当对其加锁
 * @param pid
 * @return struct process_control_block*
 */
struct process_control_block *process_find_pcb_by_pid(pid_t pid);

/**
 * @brief 将进程加入到调度器的就绪队列中
 *
 * @param pcb 进程的pcb
 *
 * @return 如果进程被成功唤醒，则返回1,如果进程正在运行，则返回0.如果pcb为NULL，则返回-EINVAL
 */
int process_wakeup(struct process_control_block *pcb);

/**
 * @brief 将进程加入到调度器的就绪队列中，并标志当前进程需要被调度
 *
 * @param pcb 进程的pcb
 */
int process_wakeup_immediately(struct process_control_block *pcb);

/**
 * @brief 进程退出时执行的函数
 *
 * @param code 返回码
 * @return ul
 */
extern ul process_do_exit(ul code);

/**
 * @brief 当子进程退出后向父进程发送通知
 *
 */
void process_exit_notify();

/**
 * @brief 初始化内核进程
 *
 * @param fn 目标程序的地址
 * @param arg 向目标程序传入的参数
 * @param flags
 * @return int
 */

pid_t kernel_thread(int (*fn)(void *), void *arg, unsigned long flags){
    kerror("FIXME: kernel_thread not implemented\n");
    while(1);
}


// 获取当前cpu id
#define proc_current_cpu_id (current_pcb->cpu_id)

extern unsigned long head_stack_start; // 导出内核层栈基地址（定义在head.S）
extern ul _stack_start;
extern void ret_from_intr(void); // 导出从中断返回的函数（定义在entry.S）

extern struct tss_struct initial_tss[MAX_CPU_NUM];
extern struct mm_struct initial_mm;
extern struct thread_struct initial_thread;
extern union proc_union initial_proc_union;
extern struct process_control_block *initial_proc[MAX_CPU_NUM];

/**
 * @brief 尝试唤醒指定的进程。
 * 本函数的行为：If (@_state & @pcb->state) @pcb->state = TASK_RUNNING.
 *
 * hint: 本函数在rust中实现，请参考rust版本的注释
 */
extern int process_try_to_wake_up(struct process_control_block *_pcb, uint64_t _state, int32_t _wake_flags);

/** @brief 当进程，满足 (@state & @pcb->state)时，唤醒进程，并设置： @pcb->state = TASK_RUNNING.
 *
 * hint: 本函数在rust中实现，请参考rust版本的注释
 * @return true 唤醒成功
 * @return false 唤醒失败
 */
extern int process_wake_up_state(struct process_control_block *pcb, uint64_t state);
void __switch_to(struct process_control_block *prev, struct process_control_block *next);
