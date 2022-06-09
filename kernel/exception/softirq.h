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

#define MAX_SOFTIRQ_NUM 64

#define TIMER_SIRQ 0         // 时钟软中断号
#define VIDEO_REFRESH_SIRQ 1 // 帧缓冲区刷新软中断

/**
 * @brief 发起软中断
 *
 */
#define raise_softirq(sirq_num)            \
    do                                     \
    {                                      \
        set_softirq_status(1 << sirq_num); \
    } while (0);

/**
 * @brief 清除软中断标志位（需要软中断处理程序手动调用）
 * 
 */
#define softirq_ack(sirq_num)        \
    do                                 \
    {                                  \
        softirq_status &= (~(1 << sirq_num)); \
    } while (0);

extern uint64_t softirq_status;

struct softirq_t
{
    void (*action)(void *data); // 软中断处理函数
    void *data;
};

struct softirq_t softirq_vector[MAX_SOFTIRQ_NUM] = {0};

/**
 * @brief 软中断注册函数
 *
 * @param irq_num 软中断号
 * @param action 响应函数
 * @param data 响应数据结构体
 */
void register_softirq(uint32_t irq_num, void (*action)(void *data), void *data);

/**
 * @brief 卸载软中断
 *
 * @param irq_num 软中断号
 */
void unregister_softirq(uint32_t irq_num);

void set_softirq_status(uint64_t status);
uint64_t get_softirq_status();

/**
 * @brief 软中断处理程序
 *
 */
void do_softirq();

void softirq_init();