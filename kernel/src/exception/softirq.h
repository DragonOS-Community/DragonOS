/**
 * @file softirq.h
 * @author fslongjin (longjin@RinGoTek.cn)
 * @brief 软中断
 * @version 0.1
 * @date 2022-04-08
 *
 * @copyright Copyright (c) 2022
 *
 */
#pragma once

#include <common/glib.h>

// ==================implementation with rust===================
extern void softirq_init();
extern void raise_softirq(uint32_t sirq_num);
extern int register_softirq(uint32_t irq_num, void (*action)(void *data), void *data);
extern int unregister_softirq(uint32_t irq_num);
extern void do_softirq();

// for temporary
#define MAX_SOFTIRQ_NUM 64
#define TIMER_SIRQ 0         // 时钟软中断号
#define VIDEO_REFRESH_SIRQ 1 // 帧缓冲区刷新软中断
