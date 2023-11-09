#include "traceback.h"
#include <common/printk.h>
#include <process/process.h>

int lookup_kallsyms(uint64_t addr, int level)
{
    const char *str = (const char *)&kallsyms_names;

    // 暴力查找符合要求的symbol
    // todo: 改用二分搜索。
    // 由于符号表使用nm -n生成，因此是按照地址升序排列的，因此可以二分
    uint64_t index = 0;
    for (index = 0; index < kallsyms_num - 1; ++index)
    {
        if (addr > kallsyms_address[index] && addr <= kallsyms_address[index + 1])
            break;
    }

    if (index < kallsyms_num) // 找到对应的函数
    {
        // 依次输出函数名称、rip离函数起始处的偏移量、函数执行的rip
        printk("function:%s() \t(+) %04d address:%#018lx\n", &str[kallsyms_names_index[index]], addr - kallsyms_address[index], addr);
        return 0;
    }
    else
        return -1;
}

/**
 * @brief 追溯内核栈调用情况
 *
 * @param regs 内核栈结构体
 */
void traceback(struct pt_regs *regs)
{
    // 先检验是否为用户态出错，若为用户态出错，则直接返回
    if (verify_area(regs->rbp, 0))
    {
        printk_color(YELLOW, BLACK, "Kernel traceback: Fault in userland. pid=%ld, rbp=%#018lx\n", rs_current_pcb_pid(), regs->rbp);
        return;
    }

    uint64_t *rbp = (uint64_t *)regs->rbp;
    printk_color(YELLOW, BLACK, "======== Kernel traceback =======\n");
    // printk("&kallsyms_address:%#018lx,kallsyms_address:%#018lx\n", &kallsyms_address, kallsyms_address);
    // printk("&kallsyms_syms_num:%#018lx,kallsyms_syms_num:%d\n", &kallsyms_num, kallsyms_num);
    // printk("&kallsyms_index:%#018lx\n", &kallsyms_names_index);
    // printk("&kallsyms_names:%#018lx,kallsyms_names:%s\n", &kallsyms_names, &kallsyms_names);

    uint64_t ret_addr = regs->rip;
    // 最大追踪10层调用栈
    for (int i = 0; i < 10; ++i)
    {
        if (lookup_kallsyms(ret_addr, i) != 0)
            break;

        // 当前栈帧的rbp的地址大于等于内核栈的rbp的时候，表明调用栈已经到头了，追踪结束。
        // 当前rbp的地址为用户空间时，直接退出
        if ((uint64_t)(rbp) >= rs_current_pcb_thread_rbp() || ((uint64_t)rbp < regs->rsp))
            break;

        printk_color(ORANGE, BLACK, "rbp:%#018lx,*rbp:%#018lx\n", rbp, *rbp);

        // 由于x86处理器在执行call指令时，先将调用返回地址压入栈中，然后再把函数的rbp入栈，最后将rsp设为新的rbp。
        // 因此，此处的rbp就是上一层的rsp，那么，*(rbp+1)得到的就是上一层函数的返回地址
        ret_addr = *(rbp + 1);
        rbp = (uint64_t *)(*rbp);
        printk("\n");
    }
    printk_color(YELLOW, BLACK, "======== Kernel traceback end =======\n");
}