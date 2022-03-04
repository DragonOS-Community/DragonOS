#pragma once

#include "../../../common/asm.h"
#include"../../../process/ptrace.h"
#include"../../../exception/irq.h"

/**
 * @brief 中断服务程序
 * 
 * @param rsp 中断栈指针
 * @param number 中断号
 */
void do_IRQ(struct pt_regs* rsp, ul number);

/**
 * @brief 初始化apic控制器
 * 
 */
void apic_init();