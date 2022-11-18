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
#include <common/wait_queue.h>
#include <filesystem/VFS/VFS.h>
#include <mm/mm-types.h>
#include <syscall/syscall.h>

#include <asm/current.h>

#include "proc-types.h"

// 设置初始进程的PCB
#define INITIAL_PROC(proc)                                                                                             \
    {                                                                                                                  \
        .state = PROC_UNINTERRUPTIBLE, .flags = PF_KTHREAD, .preempt_count = 0, .signal = 0, .cpu_id = 0,              \
        .mm = &initial_mm, .thread = &initial_thread, .addr_limit = 0xffffffffffffffff, .pid = 0, .priority = 2,       \
        .virtual_runtime = 0, .fds = {0}, .next_pcb = &proc, .prev_pcb = &proc, .parent_pcb = &proc, .exit_code = 0,    \
        .wait_child_proc_exit = 0, .worker_private = NULL, .policy = SCHED_NORMAL                                      \
    }

/**
 * @brief 任务状态段结构体
 *
 */

// 设置初始进程的tss
#define INITIAL_TSS                                                                                                    \
    {                                                                                                                  \
        .reserved0 = 0, .rsp0 = (ul)(initial_proc_union.stack + STACK_SIZE / sizeof(ul)),                              \
        .rsp1 = (ul)(initial_proc_union.stack + STACK_SIZE / sizeof(ul)),                                              \
        .rsp2 = (ul)(initial_proc_union.stack + STACK_SIZE / sizeof(ul)), .reserved1 = 0, .ist1 = 0xffff800000007c00,  \
        .ist2 = 0xffff800000007c00, .ist3 = 0xffff800000007c00, .ist4 = 0xffff800000007c00,                            \
        .ist5 = 0xffff800000007c00, .ist6 = 0xffff800000007c00, .ist7 = 0xffff800000007c00, .reserved2 = 0,            \
        .reserved3 = 0, .io_map_base_addr = 0                                                                          \
    }

#define GET_CURRENT_PCB                                                                                                \
    "movq %rsp, %rbx \n\t"                                                                                             \
    "andq $-32768, %rbx\n\t"

/**
 * @brief 切换进程上下文
 * 先把rbp和rax保存到栈中，然后将rsp和rip保存到prev的thread结构体中
 * 然后调用__switch_to切换栈，配置其他信息，最后恢复下一个进程的rax rbp。
 */

#define switch_proc(prev, next)                                                                                        \
    do                                                                                                                 \
    {                                                                                                                  \
        __asm__ __volatile__("pushq	%%rbp	\n\t"                                                                        \
                             "pushq	%%rax	\n\t"                                                                        \
                             "movq	%%rsp,	%0	\n\t"                                                                     \
                             "movq	%2,	%%rsp	\n\t"                                                                     \
                             "leaq	switch_proc_ret_addr(%%rip),	%%rax	\n\t"                                            \
                             "movq	%%rax,	%1	\n\t"                                                                     \
                             "pushq	%3		\n\t"                                                                          \
                             "jmp	__switch_to	\n\t"                                                                    \
                             "switch_proc_ret_addr:	\n\t"                                                              \
                             "popq	%%rax	\n\t"                                                                         \
                             "popq	%%rbp	\n\t"                                                                         \
                             : "=m"(prev->thread->rsp), "=m"(prev->thread->rip)                                        \
                             : "m"(next->thread->rsp), "m"(next->thread->rip), "D"(prev), "S"(next)                    \
                             : "memory");                                                                              \
    } while (0)

/**
 * @brief 初始化系统的第一个进程
 *
 */
void process_init();

/**
 * @brief fork当前进程
 *
 * @param regs 新的寄存器值
 * @param clone_flags 克隆标志
 * @param stack_start 堆栈开始地址
 * @param stack_size 堆栈大小
 * @return unsigned long
 */
unsigned long do_fork(struct pt_regs *regs, unsigned long clone_flags, unsigned long stack_start,
                      unsigned long stack_size);

/**
 * @brief 根据pid获取进程的pcb。存在对应的pcb时，返回对应的pcb的指针，否则返回NULL
 *
 * @param pid
 * @return struct process_control_block*
 */
struct process_control_block *process_find_pcb_by_pid(pid_t pid);

/**
 * @brief 将进程加入到调度器的就绪队列中
 *
 * @param pcb 进程的pcb
 */
int process_wakeup(struct process_control_block *pcb);

/**
 * @brief 将进程加入到调度器的就绪队列中，并标志当前进程需要被调度
 *
 * @param pcb 进程的pcb
 */
int process_wakeup_immediately(struct process_control_block *pcb);

/**
 * @brief 使当前进程去执行新的代码
 *
 * @param regs 当前进程的寄存器
 * @param path 可执行程序的路径
 * @param argv 参数列表
 * @param envp 环境变量
 * @return ul 错误码
 */
ul do_execve(struct pt_regs *regs, char *path, char *argv[], char *envp[]);

/**
 * @brief 释放进程的页表
 *
 * @param pcb 要被释放页表的进程
 * @return uint64_t
 */
uint64_t process_exit_mm(struct process_control_block *pcb);

/**
 * @brief 进程退出时执行的函数
 *
 * @param code 返回码
 * @return ul
 */
ul process_do_exit(ul code);

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

pid_t kernel_thread(int (*fn)(void *), void *arg, unsigned long flags);

int process_fd_alloc(struct vfs_file_t *file);

int process_release_pcb(struct process_control_block *pcb);

/**
 * @brief 切换页表
 * @param prev 前一个进程的pcb
 * @param next 下一个进程的pcb
 *
 */
#define process_switch_mm(next_pcb)                                                                                    \
    do                                                                                                                 \
    {                                                                                                                  \
        asm volatile("movq %0, %%cr3	\n\t" ::"r"(next_pcb->mm->pgd) : "memory");                                    \
    } while (0)
// flush_tlb();

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
 * @brief 给pcb设置名字
 *
 * @param pcb 需要设置名字的pcb
 * @param pcb_name 保存名字的char数组
 */
void process_set_pcb_name(struct process_control_block *pcb, const char *pcb_name);
