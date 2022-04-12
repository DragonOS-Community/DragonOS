#pragma once

#include "../common/glib.h"
#include "../common/kprint.h"
#include "../process/ptrace.h"

// 定义最大系统调用数量
#define MAX_SYSTEM_CALL_NUM 128

#define ESYSCALL_NOT_EXISTS 1

typedef unsigned long (*system_call_t)(struct pt_regs *regs);

extern void ret_from_system_call(void); // 导出从系统调用返回的函数（定义在entry.S）

/**
 * @brief 初始化系统调用模块
 *
 */
void syscall_init();

/**
 * @brief 用户态系统调用入口函数
 * 从用户态进入系统调用
 * @param syscall_id 系统调用id
 * @return long 错误码
 */
long enter_syscall(ul syscall_id, ul arg0, ul arg1, ul arg2, ul arg3, ul arg4, ul arg5, ul arg6, ul arg7);
long enter_syscall_int(ul syscall_id, ul arg0, ul arg1, ul arg2, ul arg3, ul arg4, ul arg5, ul arg6, ul arg7);

/**
 * @brief 系统调用不存在时的处理函数
 *
 * @param regs 进程3特权级下的寄存器
 * @return ul
 */
ul system_call_not_exists(struct pt_regs *regs)
{
    kerror("System call [ ID #%d ] not exists.", regs->rax);
    return ESYSCALL_NOT_EXISTS;
}

/**
 * @brief 打印字符串的系统调用
 *
 * 当arg1和arg2均为0时，打印黑底白字，否则按照指定的前景色和背景色来打印
 *
 * @param regs 寄存器
 * @param arg0 要打印的字符串
 * @param arg1 前景色
 * @param arg2 背景色
 * @return ul 返回值
 */
ul sys_printf(struct pt_regs *regs);

// 系统调用的内核入口程序
void do_syscall_int(struct pt_regs *regs, unsigned long error_code);

system_call_t system_call_table[MAX_SYSTEM_CALL_NUM] =
    {
        [0] = system_call_not_exists,
        [1] = sys_printf,
        [2 ... MAX_SYSTEM_CALL_NUM - 1] = system_call_not_exists};
