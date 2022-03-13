#include "apic.h"
#include "../../../common/kprint.h"
#include "../../../common/printk.h"
#include "../../../common/cpu.h"
#include "../../../common/glib.h"
#include "../../../exception/gate.h"
#include "../../acpi/acpi.h"

// 导出定义在irq.c中的中段门表
extern void (*interrupt_table[24])(void);

bool flag_support_apic = false;
bool flag_support_x2apic = false;
uint local_apic_version;
uint local_apic_max_LVT_entries;

static struct acpi_Multiple_APIC_Description_Table_t *madt;

/**
 * @brief 初始化io_apic
 *
 */
void apic_io_apic_init()
{
    // 初始化中断门， 中断使用第二个ist
    for (int i = 32; i <= 55; ++i)
        set_intr_gate(i, 2, interrupt_table[i - 32]);

    // 屏蔽类8259A芯片
    io_out8(0x21, 0xff);
    io_out8(0xa1, 0xff);
    kdebug("8259A Masked.");
    ul madt_addr;
    kdebug("madt_addr = %#018lx", (ul)madt_addr);
    acpi_iter_SDT(acpi_get_MADT, &madt_addr);
    madt = (struct acpi_Multiple_APIC_Description_Table_t *)madt_addr;

    kdebug("MADT->local intr controller addr=%#018lx", madt->Local_Interrupt_Controller_Address);
    kdebug("MADT->length= %d bytes", madt->header.Length);

    void *ent = (void *)(madt_addr) + sizeof(struct acpi_Multiple_APIC_Description_Table_t);
    struct apic_Interrupt_Controller_Structure_header_t *header;
    for (int i = 0; i < 17; ++i)
    {
        header = (struct apic_Interrupt_Controller_Structure_header_t *)ent;
        kdebug("[ %d ] type=%d, length=%d", i, header->type, header->length);
        if (header->type == 1)
        {
            struct acpi_IO_APIC_Structure_t *t = (struct acpi_IO_APIC_Structure_t *)ent;
            kdebug("IO apic addr = %#018lx", t->IO_APIC_Address);
        }

        ent += header->length;
    }
    apic_local_apic_init();
    sti();
}

/**
 * @brief 初始化local apic
 *
 */
void apic_local_apic_init()
{
    uint a, b, c, d;

    cpu_cpuid(1, 0, &a, &b, &c, &d);

    kdebug("CPUID 0x01, eax:%#010lx, ebx:%#010lx, ecx:%#010lx, edx:%#010lx", a, b, c, d);

    // 判断是否支持APIC和xAPIC
    if ((1 << 9) & d)
    {
        flag_support_apic = true;
        kdebug("This computer support APIC&xAPIC");
    }
    else
    {
        flag_support_apic = false;
        kerror("This computer does not support APIC&xAPIC");
        while (1)
            ;
    }

    // 判断是否支持x2APIC
    if ((1 << 21) & c)
    {
        flag_support_x2apic = true;
        kdebug("This computer support x2APIC");
    }
    else
    {
        kerror("This computer does not support x2APIC");
    }

    uint eax, edx;
    // 启用xAPIC 和x2APIC
    __asm__ __volatile__("movq  $0x1b, %%rcx   \n\t" // 读取IA32_APIC_BASE寄存器
                         "rdmsr  \n\t"
                         "bts $10,   %%rax  \n\t"
                         "bts $11,   %%rax   \n\t"
                         "wrmsr  \n\t"
                         "movq $0x1b,    %%rcx   \n\t"
                         "rdmsr  \n\t"
                         : "=a"(eax), "=d"(edx)::"memory");

    kdebug("After enable xAPIC and x2APIC: edx=%#010x, eax=%#010x", edx, eax);

    // 检测是否成功启用xAPIC和x2APIC
    if (eax & 0xc00)
        kinfo("xAPIC & x2APIC enabled!");

    // 设置SVR寄存器，开启local APIC、禁止EOI广播

    __asm__ __volatile__("movq 0x80f, %%rcx    \n\t"
                         "rdmsr  \n\t"
                         "bts $8, %%rax  \n\t"
                         "bts $12, %%rax \n\t"
                         "movq 0x80f, %%rcx    \n\t"
                         "wrmsr  \n\t"
                         "movq $0x80f , %%rcx   \n\t"
                         "rdmsr \n\t"
                         : "=a"(eax), "=d"(edx)::"memory", "rcx");

    /*
   //enable SVR[8]
    __asm__ __volatile__(	"movq 	$0x80f,	%%rcx	\n\t"
                "rdmsr	\n\t"
                "bts	$8,	%%rax	\n\t"
                "bts	$12,%%rax\n\t"
                "wrmsr	\n\t"
                "movq 	$0x80f,	%%rcx	\n\t"
                "rdmsr	\n\t"
                :"=a"(eax),"=d"(edx)
                :
                :"memory");
                */
    kdebug("After setting SVR: edx=%#010x, eax=%#010x", edx, eax);

    if (eax & 0x100)
        kinfo("APIC Software Enabled.");
    if (eax & 0x1000)
        kinfo("EOI-Broadcast Suppression Enabled.");

    // 获取Local APIC的基础信息 （参见英特尔开发手册Vol3A 10-39）
    //                          Table 10-6. Local APIC Register Address Map Supported by x2APIC
    // 获取 Local APIC ID
    // 0x802处是x2APIC ID 位宽32bits 的 Local APIC ID register
    __asm__ __volatile__("movq $0x802, %%rcx    \n\t"
                         "rdmsr  \n\t"
                         : "=a"(eax), "=d"(edx)::"memory");

    kdebug("get Local APIC ID: edx=%#010x, eax=%#010x", edx, eax);

    // 获取Local APIC Version
    // 0x803处是 Local APIC Version register
    __asm__ __volatile__("movq $0x803, %%rcx    \n\t"
                         "rdmsr  \n\t"
                         : "=a"(eax), "=d"(edx)::"memory");

    local_apic_max_LVT_entries = ((eax >> 16) & 0xff) + 1;
    local_apic_version = eax & 0xff;

    kdebug("local APIC Version:%#010x,Max LVT Entry:%#010x,SVR(Suppress EOI Broadcast):%#04x\t", local_apic_version, local_apic_max_LVT_entries, (eax >> 24) & 0x1);

    if ((eax & 0xff) < 0x10)
    {
        kdebug("82489DX discrete APIC");
    }
    else if (((eax & 0xff) >= 0x10) && ((eax & 0xff) <= 0x15))
        kdebug("Integrated APIC.");

    // 由于尚未配置LVT对应的处理程序，因此先屏蔽所有的LVT

    __asm__ __volatile__(
        "movq 	$0x82f,	%%rcx	\n\t" // CMCI
        "wrmsr	\n\t"
        "movq 	$0x832,	%%rcx	\n\t" // Timer
        "wrmsr	\n\t"
        "movq 	$0x833,	%%rcx	\n\t" // Thermal Monitor
        "wrmsr	\n\t"
        "movq 	$0x834,	%%rcx	\n\t" // Performance Counter
        "wrmsr	\n\t"
        "movq 	$0x835,	%%rcx	\n\t" // LINT0
        "wrmsr	\n\t"
        "movq 	$0x836,	%%rcx	\n\t" // LINT1
        "wrmsr	\n\t"
        "movq 	$0x837,	%%rcx	\n\t" // Error
        "wrmsr	\n\t"
        :
        : "a"(0x10000), "d"(0x00)
        : "memory");
    kdebug("All LVT Masked");

    // 获取TPR寄存器的值
    __asm__ __volatile__("movq $0x808, %%rcx    \n\t"
                         "rdmsr  \n\t"
                         : "=a"(eax), "=d"(edx)::"memory");
    kdebug("LVT_TPR=%#010x", eax);

    // 获取PPR寄存器的值
    __asm__ __volatile__("movq $0x80a, %%rcx    \n\t"
                         "rdmsr  \n\t"
                         : "=a"(eax), "=d"(edx)::"memory");
    kdebug("LVT_PPR=%#010x", eax);
}

/**
 * @brief 初始化apic控制器
 *
 */
void apic_init()
{
    apic_io_apic_init();
}
/**
 * @brief 中断服务程序
 *
 * @param rsp 中断栈指针
 * @param number 中断号
 */
void do_IRQ(struct pt_regs *rsp, ul number)
{
}