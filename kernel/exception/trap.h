/**
 * @file trap.h
 * @author longjin@RinGoTek.cn
 * @brief 异常捕获模块
 * @date 2022-01-24
 * 
 */

#pragma once

#include <common/printk.h>
#include <common/glib.h>

/**
 * @brief 初始化系统中断表
 * 
 */
void init_sys_vector();

//除法错误
void divide_error();
// 调试
void debug();
// 不可屏蔽中断
void nmi();
//
void int3();
// 溢出
void overflow();
// 边界问题
void bounds();
// 未定义的操作数
void undefined_opcode();
// 设备不可用
void dev_not_available();
void double_fault();
void coprocessor_segment_overrun();
void invalid_TSS();
void segment_not_present();
void stack_segment_fault();
void general_protection();
// 缺页异常
void page_fault();
void x87_FPU_error();
void alignment_check();
void machine_check();
void SIMD_exception();
void virtualization_exception();
