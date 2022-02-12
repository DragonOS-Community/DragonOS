/**
 * @file irq.h
 * @author longjin
 * @brief 中断处理程序
 * @version 0.1
 * @date 2022-01-28
 * 
 * @copyright Copyright (c) 2022
 * 
 */

#pragma once

#include "../common/glib.h"

#include "../process/ptrace.h"

/**
 * @brief 初始化中断模块
 */
void init_irq();


/**
 * @brief 中断服务程序
 * 
 * @param rsp 中断栈指针
 * @param number 中断号
 */
void do_IRQ(struct pt_regs* rsp, ul number);
