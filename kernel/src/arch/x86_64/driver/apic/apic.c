#include "apic.h"
#include "apic_timer.h"
#include <common/cpu.h>
#include <common/glib.h>
#include <common/kprint.h>
#include <common/printk.h>
#include <driver/acpi/acpi.h>
#include <exception/gate.h>
#include <exception/softirq.h>
#include <process/process.h>
#include <sched/sched.h>

#pragma GCC push_options
#pragma GCC optimize("O0")
// 导出定义在irq.c中的中段门表
extern void (*interrupt_table[26])(void);
extern uint32_t rs_current_pcb_preempt_count();
extern uint32_t rs_current_pcb_pid();
extern uint32_t rs_current_pcb_flags();
extern void rs_apic_init_bsp();

extern void rs_apic_local_apic_edge_ack(uint8_t irq_num);

extern int rs_ioapic_install(uint8_t vector, uint8_t dest, bool level_triggered, bool active_high, bool dest_logical);
extern void rs_ioapic_uninstall(uint8_t irq_num);
extern void rs_ioapic_enable(uint8_t irq_num);
extern void rs_ioapic_disable(uint8_t irq_num);

/**
 * @brief 初始化apic控制器
 *
 */
int apic_init()
{
    cli();
    kinfo("Initializing APIC...");
    // 初始化中断门， 中断使用rsp0防止在软中断时发生嵌套，然后处理器重新加载导致数据被抹掉
    for (int i = 32; i <= 57; ++i)
        set_intr_gate(i, 0, interrupt_table[i - 32]);

    // 设置local apic中断门
    for (int i = 150; i < 160; ++i)
        set_intr_gate(i, 0, local_apic_interrupt_table[i - 150]);

    // 初始化BSP的APIC
    rs_apic_init_bsp();

    kinfo("APIC initialized.");
    // sti();
    return 0;
}
/**
 * @brief 中断服务程序
 *
 * @param rsp 中断栈指针
 * @param number 中断向量号
 */
void do_IRQ(struct pt_regs *rsp, ul number)
{
    if((rsp->cs & 0x3) == 3)
    {
        asm volatile("swapgs":::"memory");
    }
    if (number < 0x80 && number >= 32) // 以0x80为界限，低于0x80的是外部中断控制器，高于0x80的是Local APIC
    {
        // ==========外部中断控制器========
        irq_desc_t *irq = &interrupt_desc[number - 32];

        // 执行中断上半部处理程序
        if (irq != NULL && irq->handler != NULL)
            irq->handler(number, irq->parameter, rsp);
        else
            kwarn("Intr vector [%d] does not have a handler!");
        // 向中断控制器发送应答消息
        // if (irq->controller != NULL && irq->controller->ack != NULL)
        //     irq->controller->ack(number);
        // else
        //     rs_apic_local_apic_edge_ack(number);
        rs_apic_local_apic_edge_ack(number);
    }
    else if (number >= 200)
    {
        rs_apic_local_apic_edge_ack(number);

        {
            irq_desc_t *irq = &SMP_IPI_desc[number - 200];
            if (irq->handler != NULL)
                irq->handler(number, irq->parameter, rsp);
        }
    }
    else if (number >= 150 && number < 200)
    {
        irq_desc_t *irq = &local_apic_interrupt_desc[number - 150];

        // 执行中断上半部处理程序
        if (irq != NULL && irq->handler != NULL)
            irq->handler(number, irq->parameter, rsp);
        else
            kwarn("Intr vector [%d] does not have a handler!");
        // 向中断控制器发送应答消息
        // if (irq->controller != NULL && irq->controller->ack != NULL)
        //     irq->controller->ack(number);
        // else
        //     rs_apic_local_apic_edge_ack(number);
        rs_apic_local_apic_edge_ack(number);
    }
    else
    {

        kwarn("do IRQ receive: %d", number);
        // 忽略未知中断
        return;
    }

    // kdebug("before softirq");
    // 进入软中断处理程序
    rs_do_softirq();

    // kdebug("after softirq");
    // 检测当前进程是否持有自旋锁，若持有自旋锁，则不进行抢占式的进程调度
    if (rs_current_pcb_preempt_count() > 0)
    {
        return;
    }
    else if ((int32_t)rs_current_pcb_preempt_count() < 0)
        kBUG("current_pcb->preempt_count<0! pid=%d", rs_current_pcb_pid()); // should not be here

    // 检测当前进程是否可被调度
    if ((rs_current_pcb_flags() & PF_NEED_SCHED) && number == APIC_TIMER_IRQ_NUM)
    {
        io_mfence();
        sched();
    }
}

// =========== 中断控制操作接口 ============
void apic_ioapic_enable(ul irq_num)
{
    rs_ioapic_enable(irq_num);
}

void apic_ioapic_disable(ul irq_num)
{
    rs_ioapic_disable(irq_num);
}

ul apic_ioapic_install(ul irq_num, void *arg)
{
    struct apic_IO_APIC_RTE_entry *entry = (struct apic_IO_APIC_RTE_entry *)arg;
    uint8_t dest = 0;
    if (entry->dest_mode)
    {
        dest = entry->destination.logical.logical_dest;
    }
    else
    {
        dest = entry->destination.physical.phy_dest;
    }

    return rs_ioapic_install(entry->vector, dest, entry->trigger_mode, entry->polarity, entry->dest_mode);
}

void apic_ioapic_uninstall(ul irq_num)
{
    rs_ioapic_uninstall(irq_num);
}

void apic_ioapic_edge_ack(ul irq_num) // 边沿触发
{

    rs_apic_local_apic_edge_ack(irq_num);
}

/**
 * @brief local apic 边沿触发应答
 *
 * @param irq_num
 */

void apic_local_apic_edge_ack(ul irq_num)
{
    rs_apic_local_apic_edge_ack(irq_num);
}

/**
 * @brief 构造RTE Entry结构体
 *
 * @param entry 返回的结构体
 * @param vector 中断向量
 * @param deliver_mode 投递模式
 * @param dest_mode 目标模式
 * @param deliver_status 投递状态
 * @param polarity 电平触发极性
 * @param irr 远程IRR标志位（只读）
 * @param trigger 触发模式
 * @param mask 屏蔽标志位，（0为未屏蔽， 1为已屏蔽）
 * @param dest_apicID 目标apicID
 */
void apic_make_rte_entry(struct apic_IO_APIC_RTE_entry *entry, uint8_t vector, uint8_t deliver_mode, uint8_t dest_mode,
                         uint8_t deliver_status, uint8_t polarity, uint8_t irr, uint8_t trigger, uint8_t mask,
                         uint8_t dest_apicID)
{

    entry->vector = vector;
    entry->deliver_mode = deliver_mode;
    entry->dest_mode = dest_mode;
    entry->deliver_status = deliver_status;
    entry->polarity = polarity;
    entry->remote_IRR = irr;
    entry->trigger_mode = trigger;
    entry->mask = mask;

    entry->reserved = 0;

    if (dest_mode == DEST_PHYSICAL)
    {
        entry->destination.physical.phy_dest = dest_apicID;
        entry->destination.physical.reserved1 = 0;
        entry->destination.physical.reserved2 = 0;
    }
    else
    {
        entry->destination.logical.logical_dest = dest_apicID;
        entry->destination.logical.reserved1 = 0;
    }
}

#pragma GCC pop_options