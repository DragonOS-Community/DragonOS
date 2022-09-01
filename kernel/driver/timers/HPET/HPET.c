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
#include <driver/video/video.h>
#include <driver/interrupt/apic/apic_timer.h>
#include <common/spinlock.h>

#pragma GCC push_options
#pragma GCC optimize("O0")
static struct acpi_HPET_description_table_t *hpet_table;
static uint64_t HPET_REG_BASE = 0;
static uint32_t HPET_COUNTER_CLK_PERIOD = 0; // 主计数器时间精度（单位：飞秒）
static uint64_t HPET_freq = 0;               // 主计时器频率
static uint8_t HPET_NUM_TIM_CAP = 0;         // 定时器数量
static char measure_apic_timer_flag;         // 初始化apic时钟时所用到的标志变量

// 测定tsc频率的临时变量
static uint64_t test_tsc_start = 0;
static uint64_t test_tsc_end = 0;
extern uint64_t Cpu_tsc_freq; // 导出自cpu.c

extern struct rtc_time_t rtc_now; // 导出全局墙上时钟

enum
{
    GCAP_ID = 0x00,
    GEN_CONF = 0x10,
    GINTR_STA = 0x20,
    MAIN_CNT = 0xf0,
    TIM0_CONF = 0x100,
    TIM0_COMP = 0x108,
    TIM1_CONF = 0x120,
    TIM1_COMP = 0x128,
    TIM2_CONF = 0x140,
    TIM2_COMP = 0x148,
    TIM3_CONF = 0x160,
    TIM3_COMP = 0x168,
    TIM4_CONF = 0x180,
    TIM4_COMP = 0x188,
    TIM5_CONF = 0x1a0,
    TIM5_COMP = 0x1a8,
    TIM6_CONF = 0x1c0,
    TIM6_COMP = 0x1c8,
    TIM7_CONF = 0x1e0,
    TIM7_COMP = 0x1e8,
};

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
        timer_jiffies += HPET0_INTERVAL;

        /*
        // 将HEPT中断消息转发到ap:1处理器
        ipi_send_IPI(DEST_PHYSICAL, IDLE, ICR_LEVEL_DE_ASSERT, EDGE_TRIGGER, 0xc8,
                     ICR_APIC_FIXED, ICR_ALL_EXCLUDE_Self, true, 0);
                     */

        // 若当前时间比定时任务的时间间隔大，则进入中断下半部
        if (container_of(list_next(&timer_func_head.list), struct timer_func_list_t, list)->expire_jiffies <= timer_jiffies)
            raise_softirq(TIMER_SIRQ);

        // 当时间到了，或进程发生切换时，刷新帧缓冲区
        if (timer_jiffies >= video_refresh_expire_jiffies || (video_last_refresh_pid != current_pcb->pid))
        {
            raise_softirq(VIDEO_REFRESH_SIRQ);
            // 超过130ms仍未刷新完成，则重新发起刷新(防止由于进程异常退出导致的屏幕无法刷新)
            if (unlikely(timer_jiffies >= (video_refresh_expire_jiffies + (1 << 17))))
            {
                video_refresh_expire_jiffies = timer_jiffies + (1 << 20);
                clear_softirq_pending(VIDEO_REFRESH_SIRQ);
            }
        }
        break;

    default:
        kwarn("Unsupported HPET irq: %d.", number);
        break;
    }
}

/**
 * @brief 测定apic定时器以及tsc的频率的中断回调函数
 *
 */
void HPET_measure_handler(uint64_t number, uint64_t param, struct pt_regs *regs)
{
    test_tsc_end = rdtsc();
    // 停止apic定时器
    // 写入每1ms的ticks
    apic_timer_stop();
    apic_timer_ticks_result = 0xFFFFFFFF - apic_timer_get_current();
    measure_apic_timer_flag = true;
}

/**
 * @brief 测定apic定时器以及tsc的频率
 *
 */
void HPET_measure_freq()
{
    kinfo("Measuring local APIC timer's frequency...");
    const uint64_t interval = APIC_TIMER_INTERVAL; // 测量给定时间内的计数
    struct apic_IO_APIC_RTE_entry entry;

    // 使用I/O APIC 的IRQ2接收hpet定时器0的中断
    apic_make_rte_entry(&entry, 34, IO_APIC_FIXED, DEST_PHYSICAL, IDLE, POLARITY_HIGH, IRR_RESET, EDGE_TRIGGER, MASKED, 0);

    // 计算HPET0间隔多少个时钟周期触发一次中断
    uint64_t clks_to_intr = 0.001 * interval * HPET_freq;
    // kdebug("clks_to_intr=%#ld", clks_to_intr);
    if (clks_to_intr <= 0 || clks_to_intr > (HPET_freq * 8))
    {
        kBUG("HPET0: Numof clocks to generate interrupt is INVALID! value=%lld", clks_to_intr);
        while (1)
            hlt();
    }
    __write8b(HPET_REG_BASE + MAIN_CNT, 0);
    io_mfence();
    __write8b((HPET_REG_BASE + TIM0_CONF), 0x0044); // 设置定时器0为非周期，边沿触发，默认投递到IO APIC的2号引脚
    io_mfence();
    __write8b(HPET_REG_BASE + TIM0_COMP, clks_to_intr);

    io_mfence();

    measure_apic_timer_flag = false;

    // 注册中断
    irq_register(34, &entry, &HPET_measure_handler, 0, &HPET_intr_controller, "HPET0 measure");
    sti();

    // 设置div16
    apic_timer_stop();
    apic_timer_set_div(APIC_TIMER_DIVISOR);

    // 设置初始计数
    apic_timer_set_init_cnt(0xFFFFFFFF);

    // 启动apic定时器
    apic_timer_set_LVT(151, 0, APIC_LVT_Timer_One_Shot);
    __write8b(HPET_REG_BASE + GEN_CONF, 3); // 置位旧设备中断路由兼容标志位、定时器组使能标志位，开始计时

    // 顺便测定tsc频率
    test_tsc_start = rdtsc();
    io_mfence();
    while (measure_apic_timer_flag == false)
        ;

    irq_unregister(34);

    *(uint64_t *)(HPET_REG_BASE + GEN_CONF) = 0; // 停用HPET定时器
    io_mfence();
    kinfo("Local APIC timer's freq: %d ticks/ms.", apic_timer_ticks_result);
    // 计算tsc频率
    Cpu_tsc_freq = (test_tsc_end - test_tsc_start) * (1000UL / interval);

    kinfo("TSC frequency: %ldMHz", Cpu_tsc_freq / 1000000);
}

/**
 * @brief 启用HPET周期中断（5ms）
 *
 */
void HPET_enable()
{
    struct apic_IO_APIC_RTE_entry entry;
    // 使用I/O APIC 的IRQ2接收hpet定时器0的中断
    apic_make_rte_entry(&entry, 34, IO_APIC_FIXED, DEST_PHYSICAL, IDLE, POLARITY_HIGH, IRR_RESET, EDGE_TRIGGER, MASKED, 0);

    // 计算HPET0间隔多少个时钟周期触发一次中断
    uint64_t clks_to_intr = 0.000001 * HPET0_INTERVAL * HPET_freq;
    // kdebug("clks_to_intr=%#ld", clks_to_intr);
    if (clks_to_intr <= 0 || clks_to_intr > (HPET_freq * 8))
    {
        kBUG("HPET0: Numof clocks to generate interrupt is INVALID! value=%lld", clks_to_intr);
        while (1)
            hlt();
    }
    // kdebug("[HPET0] conf register=%#018lx  conf register[63:32]=%#06lx", (*(uint64_t *)(HPET_REG_BASE + TIM0_CONF)), ((*(uint64_t *)(HPET_REG_BASE + TIM0_CONF))>>32)&0xffffffff);
    __write8b(HPET_REG_BASE + MAIN_CNT, 0);
    io_mfence();
    __write8b(HPET_REG_BASE + TIM0_CONF, 0x004c); // 设置定时器0为周期定时，边沿触发，默认投递到IO APIC的2号引脚(看conf寄存器的高32bit，哪一位被置1，则可以投递到哪一个I/O apic引脚)
    io_mfence();
    __write8b(HPET_REG_BASE + TIM0_COMP, clks_to_intr);

    io_mfence();

    // kdebug("[HPET0] conf register after modify=%#018lx", ((*(uint64_t *)(HPET_REG_BASE + TIM0_CONF))));
    // kdebug("[HPET1] conf register =%#018lx", ((*(uint64_t *)(HPET_REG_BASE + TIM1_CONF))));

    rtc_get_cmos_time(&rtc_now);

    kinfo("HPET0 enabled.");

    __write8b(HPET_REG_BASE + GEN_CONF, 3); // 置位旧设备中断路由兼容标志位、定时器组使能标志位
    io_mfence();
    // 注册中断
    irq_register(34, &entry, &HPET_handler, 0, &HPET_intr_controller, "HPET0");
}

int HPET_init()
{
    kinfo("Initializing HPET...");
    // 从acpi获取hpet结构体
    ul hpet_table_addr = 0;
    acpi_iter_SDT(acpi_get_HPET, &hpet_table_addr);

    // ACPI表没有HPET，尝试读HPTC
    if (hpet_table_addr == 0)
    {
        kwarn("ACPI: HPET Table Not Found On This Computer!");

        if (RCBA_vaddr != 0)
        {
            kerror("NO HPET found on this computer!");
            uint64_t hptc_vaddr = (RCBA_vaddr + 0x3404UL);
            // enable HPET
            io_mfence();
            // 读取HPET配置寄存器地址
            switch (__read4b(hptc_vaddr) & 0x3)
            {
            case 0:
                HPET_REG_BASE = SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + 0xfed00000;
                break;
            case 1:
                HPET_REG_BASE = SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + 0xfed01000;
                break;
            case 2:
                HPET_REG_BASE = SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + 0xfed02000;
                break;
            case 3:
                HPET_REG_BASE = SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + 0xfed03000;
                break;
            default:
                break;
            }
            // enable HPET
            __write4b(hptc_vaddr, 0x80);
            io_mfence();
        }
        else
        {
            // 没有RCBA寄存器，采用默认值
            HPET_REG_BASE = SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + 0xfed00000;
            kwarn("There is no RCBA register on this computer, and HPET regs base use default value.");
        }
    }
    else // ACPI表中有HPET表
    {
        hpet_table = (struct acpi_HPET_description_table_t *)hpet_table_addr;
        // kdebug("hpet_table_addr=%#018lx", hpet_table_addr);

        // 由于这段内存与io/apic的映射在同一物理页内，因此不需要重复映射
        HPET_REG_BASE = SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + hpet_table->address;
        kdebug("hpet_table->address=%#018lx", hpet_table->address);
    }
    kdebug("HPET_REG_BASE=%#018lx", HPET_REG_BASE);

    // 读取计时精度并计算频率
    uint64_t tmp;
    tmp = __read8b(HPET_REG_BASE + GCAP_ID);
    HPET_COUNTER_CLK_PERIOD = (tmp >> 32) & 0xffffffff;
    HPET_freq = 1e15 / HPET_COUNTER_CLK_PERIOD;
    HPET_NUM_TIM_CAP = (tmp >> 8) & 0x1f; // 读取计时器数量

    kdebug("HPET_COUNTER_CLK_PERIOD=%#018lx", HPET_COUNTER_CLK_PERIOD);
    kinfo("Total HPET timers: %d", HPET_NUM_TIM_CAP);

    kinfo("HPET driver Initialized.");
}
#pragma GCC pop_options
