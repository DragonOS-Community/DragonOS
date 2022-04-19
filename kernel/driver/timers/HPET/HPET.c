#include "HPET.h"
#include <common/kprint.h>
#include <mm/mm.h>
#include <driver/interrupt/apic/apic.h>
#include <exception/softirq.h>
#include <driver/timers/timer.h>
#include <process/process.h>
#include <sched/sched.h>
#include <smp/ipi.h>

static struct acpi_HPET_description_table_t *hpet_table;
static uint64_t HPET_REG_BASE = 0;
static uint32_t HPET_COUNTER_CLK_PERIOD = 0; // 主计数器时间精度（单位：飞秒）
static double HPET_freq = 0;                 // 主计时器频率
static uint8_t HPET_NUM_TIM_CAP = 0;         // 定时器数量

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
        ++timer_jiffies;

        /*
        // 将HEPT中断消息转发到ap:1处理器
        ipi_send_IPI(DEST_PHYSICAL, IDLE, ICR_LEVEL_DE_ASSERT, EDGE_TRIGGER, 0xc8,
                     ICR_APIC_FIXED, ICR_ALL_EXCLUDE_Self, true, 0);
                     */

        // 若当前时间比定时任务的时间间隔大，则进入中断下半部
        if (container_of(list_next(&timer_func_head.list), struct timer_func_list_t, list)->expire_jiffies <= timer_jiffies)
            set_softirq_status(TIMER_SIRQ);

        sched_update_jiffies();

        break;

    default:
        kwarn("Unsupported HPET irq: %d.", number);
        break;
    }
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
            uint32_t *hptc = (uint32_t *)(RCBA_vaddr + 0x3404UL);
            // enable HPET
            io_mfence();
            // 读取HPET配置寄存器地址
            switch ((*hptc) & 0x3)
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
            *hptc = 0x80;
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
        kdebug("hpet_table_addr=%#018lx", hpet_table_addr);

        // 由于这段内存与io/apic的映射在同一物理页内，因此不需要重复映射
        HPET_REG_BASE = SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + hpet_table->address;
    }

    // 读取计时精度并计算频率
    uint64_t tmp;
    tmp = *(uint64_t *)(HPET_REG_BASE + GCAP_ID);
    HPET_COUNTER_CLK_PERIOD = (tmp >> 32) & 0xffffffff;
    HPET_freq = 1.0 * 1e15 / HPET_COUNTER_CLK_PERIOD;
    HPET_NUM_TIM_CAP = (tmp >> 8) & 0x1f; // 读取计时器数量

    // kinfo("HPET CLK_PERIOD=%#03lx Frequency=%f", HPET_COUNTER_CLK_PERIOD, (double)HPET_freq);

    struct apic_IO_APIC_RTE_entry entry;
    // 使用I/O APIC 的IRQ2接收hpet定时器0的中断
    apic_make_rte_entry(&entry, 34, IO_APIC_FIXED, DEST_PHYSICAL, IDLE, POLARITY_HIGH, IRR_RESET, EDGE_TRIGGER, MASKED, 0);

    *(uint64_t *)(HPET_REG_BASE + MAIN_CNT) = 0;
    io_mfence();
    *(uint64_t *)(HPET_REG_BASE + TIM0_CONF) = 0x004c; // 设置定时器0为周期定时，边沿触发，投递到IO APIC的2号引脚（这里有点绕，写的是8259的引脚号，但是因为禁用了8259，因此会被路由到IO APIC的2号引脚）
    io_mfence();
    *(uint64_t *)(HPET_REG_BASE + TIM0_COMP) = HPET_freq; // 1s触发一次中断
    io_mfence();

    rtc_get_cmos_time(&rtc_now);

    kinfo("HPET Initialized.");
    *(uint64_t *)(HPET_REG_BASE + GEN_CONF) = 3; // 置位旧设备中断路由兼容标志位、定时器组使能标志位
    io_mfence();
    // 注册中断
    irq_register(34, &entry, &HPET_handler, 0, &HPET_intr_controller, "HPET0");
}