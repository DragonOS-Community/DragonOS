/**
 * @file 8259A.h
 * @author longjin
 * @brief 8259A中断芯片
 * @version 0.1
 * @date 2022-01-29
 * 
 * @copyright Copyright (c) 2022
 * 
 */

#pragma once

#include <common/glib.h>
#include <exception/irq.h>

#define PIC_EOI		0x20
#define PIC_master		0x20		/* IO base address for master PIC */
#define PIC2_slave		0xA0		/* IO base address for slave PIC */

// 初始化8259A芯片的中断服务
void init_8259A();

/**
 * @brief 中断服务程序
 * 
 * @param rsp 中断栈指针
 * @param number 中断号
 */
void do_IRQ(struct pt_regs* rsp, ul number);



