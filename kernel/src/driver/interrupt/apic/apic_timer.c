#include "apic_timer.h"
#include <common/kprint.h>
#include <exception/irq.h>
#include <process/process.h>
#include <sched/sched.h>

// #pragma GCC push_options
// #pragma GCC optimize("O0")
uint64_t apic_timer_ticks_result = 0;
static spinlock_t apic_timer_init_lock = {1};
// bsp 是否已经完成apic时钟初始化
static bool bsp_initialized = false;

extern uint64_t rs_get_cycles();
extern uint64_t rs_tsc_get_cpu_khz();
extern void rs_apic_timer_install(int irq_num);
extern void rs_apic_timer_uninstall(int irq_num);
extern void rs_apic_timer_enable(int irq_num);
extern void rs_apic_timer_disable(int irq_num);

/**
 * @brief 初始化AP核的apic时钟
 *
 */
void apic_timer_ap_core_init()
{
    while (!bsp_initialized)
    {
        pause();
    }

    apic_timer_init();
}

void apic_timer_enable(uint64_t irq_num)
{
    rs_apic_timer_enable(irq_num);
}

void apic_timer_disable(uint64_t irq_num)
{
    rs_apic_timer_disable(irq_num);
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

    rs_apic_timer_install(irq_num);
    return 0;
}

void apic_timer_uninstall(ul irq_num)
{
    // apic_timer_write_LVT(APIC_LVT_INT_MASKED);
    io_mfence();
}

hardware_intr_controller apic_timer_intr_controller = {
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

    uint64_t flags = 0;
    spin_lock_irqsave(&apic_timer_init_lock, flags);
    kinfo("Initializing apic timer for cpu %d", rs_current_pcb_cpuid());
    io_mfence();
    irq_register(APIC_TIMER_IRQ_NUM, NULL, &apic_timer_handler, 0, &apic_timer_intr_controller,
                 "apic timer");
    io_mfence();
    if (rs_current_pcb_cpuid() == 0)
    {
        bsp_initialized = true;
    }
    kdebug("apic timer init done for cpu %d", rs_current_pcb_cpuid());
    spin_unlock_irqrestore(&apic_timer_init_lock, flags);
}

void c_register_apic_timer_irq()
{
    irq_register(APIC_TIMER_IRQ_NUM, NULL, &apic_timer_handler, 0, &apic_timer_intr_controller,
                 "apic timer");
}