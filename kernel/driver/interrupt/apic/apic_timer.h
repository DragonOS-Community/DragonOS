#pragma once

#include <common/unistd.h>
#include "apic.h"

extern uint64_t apic_timer_ticks_result;
// 5ms产生一次中断
#define APIC_TIMER_INTERVAL 5
#define APIC_TIMER_DIVISOR 3

#define APIC_TIMER_IRQ_NUM 151

/**
 * @brief 设置apic定时器的分频计数
 *
 * @param divider 分频除数
 */
#define apic_timer_set_div(divider) \
    do                              \
    {                               \
        wrmsr(0x83e, divider);      \
    } while (0)

/**
 * @brief 设置apic定时器的初始计数值
 *
 * @param init_cnt 初始计数值
 */
#define apic_timer_set_init_cnt(init_cnt) \
    do                                    \
    {                                     \
        wrmsr(0x838, init_cnt);           \
    } while (0)

/**
 * @brief 设置apic定时器的lvt，并启动定时器
 *
 * @param vector 中断向量号
 * @param mask 是否屏蔽（1：屏蔽， 0：不屏蔽）
 * @param mode 计时模式
 */
#define apic_timer_set_LVT(vector, mask, mode)                                    \
    do                                                                            \
    {                                                                             \
        wrmsr(0x832, (mode << 17) | vector | (mask ? (APIC_LVT_INT_MASKED) : 0)); \
    } while (0)

#define apic_timer_write_LVT(value) \
    do                              \
    {                               \
        wrmsr(0x832, value);        \
    } while (0)

/**
 * @brief 获取apic定时器的LVT的值
 *
 */
#define apic_timer_get_LVT() (rdmsr(0x832))
/**
 * @brief 获取apic定时器当前计数值
 *
 */
#define apic_timer_get_current() (rdmsr(0x839))

/**
 * @brief 停止apic定时器
 *
 */
#define apic_timer_stop()                    \
    do                                       \
    {                                        \
        uint64_t val = apic_timer_get_LVT(); \
        val |= APIC_LVT_INT_MASKED;          \
        apic_timer_write_LVT(val);           \
    } while (0)

/**
 * @brief 初始化local APIC定时器
 *
 */
void apic_timer_init();