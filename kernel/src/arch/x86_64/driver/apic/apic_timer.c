#include "apic_timer.h"
#include <common/kprint.h>
#include <exception/irq.h>
#include <process/process.h>
#include <sched/sched.h>


// bsp 是否已经完成apic时钟初始化
static bool bsp_initialized = false;

extern void rs_apic_timer_install(int irq_num);
extern void rs_apic_timer_uninstall(int irq_num);
extern void rs_apic_timer_enable(int irq_num);
extern void rs_apic_timer_disable(int irq_num);
extern int rs_apic_timer_handle_irq();

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
    rs_apic_timer_uninstall(irq_num);
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
    rs_apic_timer_handle_irq();
}

/**
 * @brief 初始化local APIC定时器
 *
 */
void apic_timer_init()
{
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
}
