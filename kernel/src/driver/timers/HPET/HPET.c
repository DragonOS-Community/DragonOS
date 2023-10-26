#include "HPET.h"
#include <common/kprint.h>
#include <common/compiler.h>
#include <mm/mm.h>
#include <driver/interrupt/apic/apic.h>
#include <exception/softirq.h>
#include <time/timer.h>
#include <process/process.h>
#include <sched/sched.h>
#include <smp/ipi.h>
#include <driver/interrupt/apic/apic_timer.h>
#include <common/spinlock.h>
#include <process/preempt.h>

#pragma GCC push_options
#pragma GCC optimize("O0")

extern uint64_t rs_update_timer_jiffies(uint64_t);

hardware_intr_controller HPET_intr_controller =
    {
        .enable = apic_ioapic_enable,
        .disable = apic_ioapic_disable,
        .install = apic_ioapic_install,
        .uninstall = apic_ioapic_uninstall,
        .ack = apic_ioapic_edge_ack,
};

void HPET_handler(uint64_t number, uint64_t param, struct pt_regs *regs)
{
    // printk("(HPET)");
    switch (param)
    {
    case 0: // 定时器0中断
        rs_update_timer_jiffies(500);

        /*
        // 将HEPT中断消息转发到ap:1处理器
        ipi_send_IPI(DEST_PHYSICAL, IDLE, ICR_LEVEL_DE_ASSERT, EDGE_TRIGGER, 0xc8,
                     ICR_APIC_FIXED, ICR_ALL_EXCLUDE_Self, true, 0);
                     */

        // 若当前时间比定时任务的时间间隔大，则进入中断下半部
        if (rs_timer_get_first_expire() <= rs_clock())
            rs_raise_softirq(TIMER_SIRQ);

        break;

    default:
        kwarn("Unsupported HPET irq: %d.", number);
        break;
    }
}

void c_hpet_register_irq()
{
    struct apic_IO_APIC_RTE_entry entry;
    apic_make_rte_entry(&entry, 34, IO_APIC_FIXED, DEST_PHYSICAL, IDLE, POLARITY_HIGH, IRR_RESET, EDGE_TRIGGER, MASKED, 0);
    irq_register(34, &entry, &HPET_handler, 0, &HPET_intr_controller, "HPET0");
}

#pragma GCC pop_options
