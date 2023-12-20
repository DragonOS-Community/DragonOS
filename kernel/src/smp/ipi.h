#pragma once

#include <arch/arch.h>
#if ARCH(I386) || ARCH(X86_64)
#include <arch/x86_64/include/x86_64_ipi.h>

/**
 * @brief ipi中断处理注册函数
 *
 * @param irq_num 中断向量号
 * @param arg 参数
 * @param handler 处理函数
 * @param param 参数
 * @param controller 当前为NULL
 * @param irq_name ipi中断名
 * @return int 成功：0
 */
extern int ipi_regiserIPI(uint64_t irq_num, void *arg,
                          void (*handler)(uint64_t irq_num, uint64_t param, struct pt_regs *regs),
                          uint64_t param, hardware_intr_controller *controller, char *irq_name);

#else
int ipi_regiserIPI(uint64_t irq_num, void *arg,
                   void (*handler)(uint64_t irq_num, uint64_t param, struct pt_regs *regs),
                   uint64_t param, hardware_intr_controller *controller, char *irq_name)
{
    return -1;
}

#endif