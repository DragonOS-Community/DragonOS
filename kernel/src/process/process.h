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
#include <syscall/syscall.h>

/**
 * @brief 进程退出时执行的函数
 *
 * @param code 返回码
 * @return ul
 */
extern ul rs_process_do_exit(ul code);

extern int rs_current_cpu_id();

extern unsigned long head_stack_start; // 导出内核层栈基地址（定义在head.S）
extern ul _stack_start;
extern void ret_from_intr(void); // 导出从中断返回的函数（定义在entry.S）

extern uint32_t rs_current_pcb_cpuid();
extern uint32_t rs_current_pcb_pid();
extern uint32_t rs_current_pcb_preempt_count();
extern uint32_t rs_current_pcb_flags();
extern int64_t rs_current_pcb_thread_rbp();

#define PF_NEED_SCHED (1UL << 1)
