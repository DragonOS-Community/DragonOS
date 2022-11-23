#pragma once

#include <common/unistd.h>
#include "apic.h"

extern uint64_t apic_timer_ticks_result;
// 5ms产生一次中断
#define APIC_TIMER_INTERVAL 5
#define APIC_TIMER_DIVISOR 3

#define APIC_TIMER_IRQ_NUM 151

#pragma GCC push_options
#pragma GCC optimize("O0")

/**
 * @brief 设置apic定时器的分频计数
 *
 * @param divider 分频除数
 */
static __always_inline void apic_timer_set_div(uint64_t divider)
{
    if (CURRENT_APIC_STATE == APIC_X2APIC_ENABLED)
        wrmsr(0x83e, divider);
    else
        __write4b(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_CLKDIV, divider);
}

/**
 * @brief 设置apic定时器的初始计数值
 *
 * @param init_cnt 初始计数值
 */
static __always_inline void apic_timer_set_init_cnt(uint32_t init_cnt)
{
    if (CURRENT_APIC_STATE == APIC_X2APIC_ENABLED)
        wrmsr(0x838, init_cnt);
    else
        __write4b(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_INITIAL_COUNT_REG, init_cnt);
}

/**
 * @brief 设置apic定时器的lvt，并启动定时器
 *
 * @param vector 中断向量号
 * @param mask 是否屏蔽（1：屏蔽， 0：不屏蔽）
 * @param mode 计时模式
 */
static __always_inline void apic_timer_set_LVT(uint32_t vector, uint32_t mask, uint32_t mode)
{
    register uint32_t val = (mode << 17) | vector | (mask ? (APIC_LVT_INT_MASKED) : 0);
    if (CURRENT_APIC_STATE == APIC_X2APIC_ENABLED)
        wrmsr(0x832, val);
    else
        __write4b(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_LVT_TIMER, val);
}

static __always_inline void apic_timer_write_LVT(uint32_t value)
{
    if (CURRENT_APIC_STATE == APIC_X2APIC_ENABLED)
        wrmsr(0x832, value);
    else
        __write4b(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_LVT_TIMER, value);
}

/**
 * @brief 获取apic定时器的LVT的值
 *
 */
static __always_inline uint32_t apic_timer_get_LVT()
{
    if (CURRENT_APIC_STATE == APIC_X2APIC_ENABLED)
        return rdmsr(0x832);
    else
        return __read4b(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_LVT_TIMER);
}

/**
 * @brief 获取apic定时器当前计数值
 *
 */
static __always_inline uint32_t apic_timer_get_current()
{
    if (CURRENT_APIC_STATE == APIC_X2APIC_ENABLED)
        return (uint32_t)rdmsr(0x839);
    else
        return __read4b(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_CURRENT_COUNT_REG);
}

/**
 * @brief 停止apic定时器
 *
 */
#define apic_timer_stop()                    \
    do                                       \
    {                                        \
        uint32_t val = apic_timer_get_LVT(); \
        val |= APIC_LVT_INT_MASKED;          \
        apic_timer_write_LVT(val);           \
    } while (0)

/**
 * @brief 初始化local APIC定时器
 *
 */
void apic_timer_init();

#pragma GCC pop_options