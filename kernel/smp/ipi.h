#pragma once

#ifdef x86_64
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
                         uint32_t vector, uint32_t deliver_mode, uint32_t dest_shorthand, bool apic_type, uint32_t destination);
