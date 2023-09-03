#include "syscall.h"
#include <common/errno.h>
#include <common/fcntl.h>
#include <common/kthread.h>
#include <common/string.h>
#include <driver/disk/ahci/ahci.h>
#include <exception/gate.h>
#include <exception/irq.h>
#include <filesystem/vfs/VFS.h>
#include <mm/slab.h>
#include <process/process.h>
#include <time/sleep.h>
// 导出系统调用入口函数，定义在entry.S中
extern void syscall_int(void);

/**
 * @brief 重新定义为：把系统调用函数加入系统调用表
 * @param syscall_num 系统调用号
 * @param symbol 系统调用处理函数
 */
#define SYSCALL_COMMON(syscall_num, symbol) [syscall_num] = symbol,

/**
 * @brief 通过中断进入系统调用
 *
 * @param syscall_id
 * @param arg0
 * @param arg1
 * @param arg2
 * @param arg3
 * @param arg4
 * @param arg5
 * @param arg6
 * @param arg7
 * @return long
 */

long enter_syscall_int(ul syscall_id, ul arg0, ul arg1, ul arg2, ul arg3, ul arg4, ul arg5, ul arg6, ul arg7)
{
    long err_code;
    __asm__ __volatile__("movq %2, %%r8 \n\t"
                         "movq %3, %%r9 \n\t"
                         "movq %4, %%r10 \n\t"
                         "movq %5, %%r11 \n\t"
                         "movq %6, %%r12 \n\t"
                         "movq %7, %%r13 \n\t"
                         "movq %8, %%r14 \n\t"
                         "movq %9, %%r15 \n\t"
                         "int $0x80   \n\t"
                         : "=a"(err_code)
                         : "a"(syscall_id), "m"(arg0), "m"(arg1), "m"(arg2), "m"(arg3), "m"(arg4), "m"(arg5), "m"(arg6),
                           "m"(arg7)
                         : "memory", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "rcx", "rdx");

    return err_code;
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
ul do_put_string(char *s, uint32_t front_color, uint32_t background_color)
{

    printk_color(front_color, background_color, s);
    return 0;
}

/**
 * @brief 等待进程退出
 *
 * @param pid 目标进程id
 * @param status 返回的状态信息
 * @param options 等待选项
 * @param rusage
 * @return uint64_t
 */
uint64_t c_sys_wait4(pid_t pid, int *status, int options, void *rusage)
{

    return 0;
}
