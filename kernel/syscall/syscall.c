#include "syscall.h"
#include "../process/process.h"

// 导出系统调用入口函数，定义在entry.S中
extern void system_call(void);

/**
 * @brief 系统调用函数，从entry.S中跳转到这里
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
    // 向MSR寄存器组中的 IA32_SYSENTER_CS寄存器写入内核的代码段的地址
    wrmsr(0x174, KERNEL_CS);
    // 向MSR寄存器组中的 IA32_SYSENTER_ESP寄存器写入内核进程的rbp（在syscall入口中会将rsp减去相应的数值）
    wrmsr(0x175, current_pcb->thread->rbp);
    

    // 向MSR寄存器组中的 IA32_SYSENTER_EIP寄存器写入系统调用入口的地址。
    wrmsr(0x176, (ul)system_call);
    
}

long enter_syscall(ul syscall_id, ul arg0, ul arg1, ul arg2, ul arg3, ul arg4, ul arg5, ul arg6, ul arg7)
{
    long err_code;
    __asm__ __volatile__("leaq sysexit_return_address(%%rip), %%rdx   \n\t"
                         "movq %%rsp, %%rcx      \n\t"
                         "movq %2, %%r8 \n\t"
                         "movq %3, %%r9 \n\t"
                         "movq %4, %%r10 \n\t"
                         "movq %5, %%r11 \n\t"
                         "movq %6, %%r12 \n\t"
                         "movq %7, %%r13 \n\t"
                         "movq %8, %%r14 \n\t"
                         "movq %9, %%r15 \n\t"
                         "sysenter   \n\t"
                         "sysexit_return_address:    \n\t"
                         : "=a"(err_code)
                         : "0"(syscall_id), "m"(arg0), "m"(arg1), "m"(arg2), "m"(arg3), "m"(arg4), "m"(arg5), "m"(arg6), "m"(arg7)
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
    
    //if(regs->r9 == 0 &&regs->r10 == 0)
    //    printk((char*)regs->r8);
    //else printk_color(regs->r9, regs->r10, (char*)regs->r8);
    printk_color(BLACK,WHITE,(char *)regs->rdi);

    return 0;
}