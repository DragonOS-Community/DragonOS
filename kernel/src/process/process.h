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
