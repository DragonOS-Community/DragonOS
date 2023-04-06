#pragma once

#include <common/glib.h>
#include <common/kprint.h>
#include <common/unistd.h>
#include <process/ptrace.h>

// 定义最大系统调用数量
#define MAX_SYSTEM_CALL_NUM 256

#define ESYSCALL_NOT_EXISTS 1

typedef unsigned long (*system_call_t)(struct pt_regs *regs);

extern system_call_t system_call_table[MAX_SYSTEM_CALL_NUM];

// 判断系统调用是否来自用户态
#define SYSCALL_FROM_USER(regs) (user_mode(regs))
// 判断系统调用是否来自内核态
#define SYSCALL_FROM_KERNEL(regs) (!SYSCALL_FROM_USER(regs))

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
long enter_syscall_int(ul syscall_id, ul arg0, ul arg1, ul arg2, ul arg3, ul arg4, ul arg5, ul arg6, ul arg7);

/**
 * @brief 系统调用不存在时的处理函数
 *
 * @param regs 进程3特权级下的寄存器
 * @return ul
 */
ul system_call_not_exists(struct pt_regs *regs);

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

/**
 * @brief 将堆内存调整为arg0
 *
 * @param arg0 新的堆区域的结束地址
 * arg0=0  ===> 返回堆区域的起始地址
 * arg0=-1  ===> 返回堆区域的结束地址
 * @return uint64_t 错误码
 *
 */
uint64_t sys_brk(struct pt_regs *regs);

/**
 * @brief 将堆内存空间加上offset（注意，该系统调用只应在普通进程中调用，而不能是内核线程）
 *
 * @param arg0 offset偏移量
 * @return uint64_t the previous program break
 */
uint64_t sys_sbrk(struct pt_regs *regs);

/**
 * @brief 创建文件夹
 * 在VFS.c中实现
 * @param path(r8) 路径
 * @param mode(r9) 模式
 * @return uint64_t
 */
uint64_t sys_mkdir(struct pt_regs *regs);

/**
 * @brief 创建管道
 * 在pipe.c中实现
 * @param fd(r8) 文件句柄指针
 * @param num(r9) 文件句柄个数
 * @return uint64_t
 */
uint64_t sys_pipe(struct pt_regs *regs);

ul sys_ahci_end_req(struct pt_regs *regs);

// 系统调用的内核入口程序
void do_syscall_int(struct pt_regs *regs, unsigned long error_code);
