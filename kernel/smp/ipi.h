#pragma once

#include <arch/arch.h>
#if ARCH(I386) || ARCH(X86_64)
#include <arch/x86_64/x86_64_ipi.h>
#else
#error "error type of arch!"
#endif

/**
 * @brief 发送ipi消息
 *
 * @param dest_mode 目标模式
 * @param deliver_status 投递模式
 * @param level 信号驱动电平
 * @param trigger 触发模式
 * @param vector 中断向量
 * @param deliver_mode 投递模式
 * @param dest_shorthand 投递目标速记值
 * @param apic_type apic的类型 （0:xapic 1: x2apic）
 * @param destination 投递目标
 */
extern void ipi_send_IPI(uint32_t dest_mode, uint32_t deliver_status, uint32_t level, uint32_t trigger,
                         uint32_t vector, uint32_t deliver_mode, uint32_t dest_shorthand, uint32_t destination);

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