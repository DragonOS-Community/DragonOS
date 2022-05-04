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

#include "../common/cpu.h"
#include "../common/glib.h"
#include "../mm/mm.h"
#include "../syscall/syscall.h"
#include "ptrace.h"
#include <common/errno.h>
#include <filesystem/VFS/VFS.h>

// 进程最大可拥有的文件描述符数量
#define PROC_MAX_FD_NUM 16

// 进程的内核栈大小 32K
#define STACK_SIZE 32768

// 进程的运行状态
// 正在运行
#define PROC_RUNNING (1 << 0)
// 可被中断
#define PROC_INTERRUPTIBLE (1 << 1)
// 不可被中断
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
#define CLONE_FS (1 << 0) // 在进程间共享打开的文件
#define CLONE_SIGNAL (1 << 1)
#define CLONE_VM (1 << 2) // 在进程间共享虚拟内存空间

/**
 * @brief 内存空间分布结构体
 * 包含了进程内存空间分布的信息
 */
struct mm_struct
{
	pml4t_t *pgd; // 内存页表指针
	// 代码段空间
	ul code_addr_start, code_addr_end;
	// 数据段空间
	ul data_addr_start, data_addr_end;
	// 只读数据段空间
	ul rodata_addr_start, rodata_addr_end;
	// BSS段的空间
	uint64_t bss_start, bss_end;
	// 动态内存分配区（堆区域）
	ul brk_start, brk_end;
	// 应用层栈基地址
	ul stack_start;
};

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
#define PF_KTHREAD (1UL << 0)	 // 内核线程
#define PF_NEED_SCHED (1UL << 1) // 进程需要被调度
#define PF_VFORK (1UL << 2)		 // 标志进程是否由于vfork而存在资源共享

/**
 * @brief 进程控制块
 *
 */
struct process_control_block
{
	// 进程的状态
	volatile long state;
	// 进程标志：进程、线程、内核线程
	unsigned long flags;
	int64_t preempt_count; // 持有的自旋锁的数量
	long signal;
	long cpu_id; // 当前进程在哪个CPU核心上运行
	// 内存空间分布结构体， 记录内存页表和程序段信息
	struct mm_struct *mm;

	// 进程切换时保存的状态信息
	struct thread_struct *thread;

	// 连接各个pcb的双向链表（todo：删除这个变量）
	struct List list;

	// 地址空间范围
	// 用户空间： 0x0000 0000 0000 0000 ~ 0x0000 7fff ffff ffff
	// 内核空间： 0xffff 8000 0000 0000 ~ 0xffff ffff ffff ffff
	uint64_t addr_limit;

	long pid;
	long priority;		  // 优先级
	long virtual_runtime; // 虚拟运行时间

	// 进程拥有的文件描述符的指针数组
	// todo: 改用动态指针数组
	struct vfs_file_t *fds[PROC_MAX_FD_NUM];

	// 链表中的下一个pcb
	struct process_control_block *next_pcb;
	// 父进程的pcb
	struct process_control_block *parent_pcb;
};

// 将进程的pcb和内核栈融合到一起,8字节对齐
union proc_union
{
	struct process_control_block pcb;
	ul stack[STACK_SIZE / sizeof(ul)];
} __attribute__((aligned(8)));

// 设置初始进程的PCB
#define INITIAL_PROC(proc)                \
	{                                     \
		.state = PROC_UNINTERRUPTIBLE,    \
		.flags = PF_KTHREAD,              \
		.mm = &initial_mm,                \
		.thread = &initial_thread,        \
		.addr_limit = 0xffff800000000000, \
		.pid = 0,                         \
		.virtual_runtime = 0,             \
		.signal = 0,                      \
		.priority = 2,                    \
		.preempt_count = 0,               \
		.cpu_id = 0,                      \
		.fds = {0},                       \
		.next_pcb = &proc,                \
		.parent_pcb = &proc               \
	}

/**
 * @brief 任务状态段结构体
 *
 */
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

// 设置初始进程的tss
#define INITIAL_TSS                                                       \
	{                                                                     \
		.reserved0 = 0,                                                   \
		.rsp0 = (ul)(initial_proc_union.stack + STACK_SIZE / sizeof(ul)), \
		.rsp1 = (ul)(initial_proc_union.stack + STACK_SIZE / sizeof(ul)), \
		.rsp2 = (ul)(initial_proc_union.stack + STACK_SIZE / sizeof(ul)), \
		.reserved1 = 0,                                                   \
		.ist1 = 0xffff800000007c00,                                       \
		.ist2 = 0xffff800000007c00,                                       \
		.ist3 = 0xffff800000007c00,                                       \
		.ist4 = 0xffff800000007c00,                                       \
		.ist5 = 0xffff800000007c00,                                       \
		.ist6 = 0xffff800000007c00,                                       \
		.ist7 = 0xffff800000007c00,                                       \
		.reserved2 = 0,                                                   \
		.reserved3 = 0,                                                   \
		.io_map_base_addr = 0                                             \
	}

// 获取当前的pcb
struct process_control_block *get_current_pcb()
{
	struct process_control_block *current = NULL;
	// 利用了当前pcb和栈空间总大小为32k大小对齐，将rsp低15位清空，即可获得pcb的起始地址
	__asm__ __volatile__("andq %%rsp, %0   \n\t"
						 : "=r"(current)
						 : "0"(~32767UL));
	return current;
};

#define current_pcb get_current_pcb()

#define GET_CURRENT_PCB    \
	"movq %rsp, %rbx \n\t" \
	"andq $-32768, %rbx\n\t"

	/**
	 * @brief 切换进程上下文
	 * 先把rbp和rax保存到栈中，然后将rsp和rip保存到prev的thread结构体中
	 * 然后调用__switch_to切换栈，配置其他信息，最后恢复下一个进程的rax rbp。
	 */

#define switch_proc(prev, next)                                                                     \
	do                                                                                              \
	{                                                                                               \
		__asm__ __volatile__("pushq	%%rbp	\n\t"                                                     \
							 "pushq	%%rax	\n\t"                                                     \
							 "movq	%%rsp,	%0	\n\t"                                                  \
							 "movq	%2,	%%rsp	\n\t"                                                  \
							 "leaq	switch_proc_ret_addr(%%rip),	%%rax	\n\t"                         \
							 "movq	%%rax,	%1	\n\t"                                                  \
							 "pushq	%3		\n\t"                                                       \
							 "jmp	__switch_to	\n\t"                                                 \
							 "switch_proc_ret_addr:	\n\t"                                           \
							 "popq	%%rax	\n\t"                                                      \
							 "popq	%%rbp	\n\t"                                                      \
							 : "=m"(prev->thread->rsp), "=m"(prev->thread->rip)                     \
							 : "m"(next->thread->rsp), "m"(next->thread->rip), "D"(prev), "S"(next) \
							 : "memory");                                                           \
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
unsigned long do_fork(struct pt_regs *regs, unsigned long clone_flags, unsigned long stack_start, unsigned long stack_size);

/**
 * @brief 根据pid获取进程的pcb
 *
 * @param pid
 * @return struct process_control_block*
 */
struct process_control_block *process_get_pcb(long pid);

/**
 * @brief 切换页表
 * @param prev 前一个进程的pcb
 * @param next 下一个进程的pcb
 *
 */
#define process_switch_mm(prev, next)                              \
	do                                                             \
	{                                                              \
		asm volatile("movq %0, %%cr3	\n\t" ::"r"(next->mm->pgd) \
					 : "memory");                                  \
	} while (0)

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