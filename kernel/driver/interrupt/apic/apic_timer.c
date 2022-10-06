#include "apic_timer.h"
#include <exception/irq.h>
#include <process/process.h>
#include <common/kprint.h>
#include <sched/sched.h>

// #pragma GCC push_options
// #pragma GCC optimize("O0")
uint64_t apic_timer_ticks_result = 0;

void apic_timer_enable(uint64_t irq_num)
{
    // 启动apic定时器
    io_mfence();
    uint64_t val = apic_timer_get_LVT();
    io_mfence();
    val &= (~APIC_LVT_INT_MASKED);
    io_mfence();
    apic_timer_write_LVT(val);
    io_mfence();
}

void apic_timer_disable(uint64_t irq_num)
{
    apic_timer_stop();
}

/**
 * @brief 安装local apic定时器中断
 *
 * @param irq_num 中断向量号
 * @param arg 初始计数值
 * @return uint64_t
 */
uint64_t apic_timer_install(ul irq_num, void *arg)
{
    // 设置div16
    io_mfence();
    apic_timer_stop();
    io_mfence();
    apic_timer_set_div(APIC_TIMER_DIVISOR);
    io_mfence();

    // 设置初始计数
    apic_timer_set_init_cnt(*(uint64_t *)arg);
    io_mfence();
    // 填写LVT
    apic_timer_set_LVT(APIC_TIMER_IRQ_NUM, 1, APIC_LVT_Timer_Periodic);
    io_mfence();
}

void apic_timer_uninstall(ul irq_num)
{
    apic_timer_write_LVT(APIC_LVT_INT_MASKED);
    io_mfence();
}

hardware_intr_controller apic_timer_intr_controller =
    {
        .enable = apic_timer_enable,
        .disable = apic_timer_disable,
        .install = apic_timer_install,
        .uninstall = apic_timer_uninstall,
        .ack = apic_local_apic_edge_ack,
};

/**
 * @brief local apic定时器的中断处理函数
 *
 * @param number 中断向量号
 * @param param 参数
 * @param regs 寄存器值
 */
void apic_timer_handler(uint64_t number, uint64_t param, struct pt_regs *regs)
{
    io_mfence();
    sched_update_jiffies();
    io_mfence();
}

/**
 * @brief 初始化local APIC定时器
 *
 */
void apic_timer_init()
{
    if (apic_timer_ticks_result == 0)
    {
        kBUG("APIC timer ticks in 5ms is equal to ZERO!");
        while (1)
            hlt();
    }
    kinfo("Initializing apic timer for cpu %d", proc_current_cpu_id);
    io_mfence();
    irq_register(APIC_TIMER_IRQ_NUM, &apic_timer_ticks_result, &apic_timer_handler, 0, &apic_timer_intr_controller, "apic timer");
    io_mfence();
    // kinfo("Successfully initialized apic timer for cpu %d", proc_current_cpu_id);
}