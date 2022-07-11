#pragma once

#include <common/unistd.h>
#include "apic.h"

extern uint64_t apic_timer_ticksIn1ms;

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
 * @brief 停止apic定时器
 * 
 */
#define apic_timer_stop()                  \
    do                                     \
    {                                      \
        wrmsr(0x832, APIC_LVT_INT_MASKED); \
    } while (0)

/**
 * @brief 设置apic定时器的lvt，并启动定时器
 *
 */
#define apic_timer_set_LVT(vector, mode)     \
    do                                       \
    {                                        \
        wrmsr(0x832, (mode << 17) | vector); \
        io_mfence();                         \
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