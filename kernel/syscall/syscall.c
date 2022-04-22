#include "syscall.h"
#include "../process/process.h"
#include <exception/gate.h>
#include <exception/irq.h>
#include <driver/disk/ahci/ahci.h>

// 导出系统调用入口函数，定义在entry.S中
extern void system_call(void);
extern void syscall_int(void);

/**
 * @brief 导出系统调用处理函数的符号
 * 
 */
#define SYSCALL_COMMON(syscall_num, symbol) extern unsigned long symbol(struct pt_regs *regs);
SYSCALL_COMMON(0, system_call_not_exists);  // 导出system_call_not_exists函数
#undef SYSCALL_COMMON   // 取消前述宏定义

/**
 * @brief 重新定义为：把系统调用函数加入系统调用表
 * @param syscall_num 系统调用号
 * @param symbol 系统调用处理函数
 */
#define SYSCALL_COMMON(syscall_num, symbol) [syscall_num] = symbol,



/**
 * @brief sysenter的系统调用函数，从entry.S中跳转到这里
 *
 * @param regs 3特权级下的寄存器值,rax存储系统调用号
 * @return ul 对应的系统调用函数的地址
 */
ul system_call_function(struct pt_regs *regs)
{
    return system_call_table[regs->rax](regs);
}

/**
 * @brief 初始化系统调用模块
 *
 */
void syscall_init()
{
    kinfo("Initializing syscall...");

    set_system_trap_gate(0x80, 0, syscall_int); // 系统调用门
}

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
    __asm__ __volatile__(
        "movq %2, %%r8 \n\t"
        "movq %3, %%r9 \n\t"
        "movq %4, %%r10 \n\t"
        "movq %5, %%r11 \n\t"
        "movq %6, %%r12 \n\t"
        "movq %7, %%r13 \n\t"
        "movq %8, %%r14 \n\t"
        "movq %9, %%r15 \n\t"
        "int $0x80   \n\t"
        : "=a"(err_code)
        : "a"(syscall_id), "m"(arg0), "m"(arg1), "m"(arg2), "m"(arg3), "m"(arg4), "m"(arg5), "m"(arg6), "m"(arg7)
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
ul sys_printf(struct pt_regs *regs)
{

    if (regs->r9 == 0 && regs->r10 == 0)
        printk((char *)regs->r8);
    else
        printk_color(regs->r9, regs->r10, (char *)regs->r8);
    // printk_color(BLACK, WHITE, (char *)regs->r8);

    return 0;
}

ul sys_ahci_end_req(struct pt_regs *regs)
{
    ahci_end_request();
    return 0;
}

// 系统调用的内核入口程序
void do_syscall_int(struct pt_regs *regs, unsigned long error_code)
{

    ul ret = system_call_table[regs->rax](regs);
    regs->rax = ret; // 返回码
}


system_call_t system_call_table[MAX_SYSTEM_CALL_NUM] =
    {
        [0] = system_call_not_exists,
        [1] = sys_printf,
        [2 ... 254] = system_call_not_exists,
        [255] = sys_ahci_end_req};
