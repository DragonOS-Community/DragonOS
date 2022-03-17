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
static struct acpi_IO_APIC_Structure_t *io_apic_ICS;
/**
 * @brief 初始化io_apic
 *
 */
void apic_io_apic_init()
{

    ul madt_addr;
    acpi_iter_SDT(acpi_get_MADT, &madt_addr);
    kdebug("madt_addr = %#018lx", (ul)madt_addr);
    madt = (struct acpi_Multiple_APIC_Description_Table_t *)madt_addr;

    kdebug("MADT->local intr controller addr=%#018lx", madt->Local_Interrupt_Controller_Address);
    kdebug("MADT->length= %d bytes", madt->header.Length);

    // 寻找io apic的ICS
    void *ent = (void *)(madt_addr) + sizeof(struct acpi_Multiple_APIC_Description_Table_t);
    struct apic_Interrupt_Controller_Structure_header_t *header = (struct apic_Interrupt_Controller_Structure_header_t *)ent;
    while (header->length > 2)
    {
        header = (struct apic_Interrupt_Controller_Structure_header_t *)ent;
        if (header->type == 1)
        {
            struct acpi_IO_APIC_Structure_t *t = (struct acpi_IO_APIC_Structure_t *)ent;
            kdebug("IO apic addr = %#018lx", t->IO_APIC_Address);
            io_apic_ICS = t;
            break;
        }

        ent += header->length;
    }
    kdebug("Global_System_Interrupt_Base=%d", io_apic_ICS->Global_System_Interrupt_Base);

    apic_ioapic_map.addr_phys = io_apic_ICS->IO_APIC_Address;
    apic_ioapic_map.virtual_index_addr = (unsigned char *)APIC_IO_APIC_VIRT_BASE_ADDR;
    apic_ioapic_map.virtual_data_addr = (uint *)(APIC_IO_APIC_VIRT_BASE_ADDR + 0x10);
    apic_ioapic_map.virtual_EOI_addr = (uint *)(APIC_IO_APIC_VIRT_BASE_ADDR + 0x40);

    // 填写页表，完成地址映射
    mm_map_phys_addr((ul)apic_ioapic_map.virtual_index_addr, apic_ioapic_map.addr_phys, PAGE_2M_SIZE, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD);

    // 设置IO APIC ID 为0x0f000000
    *apic_ioapic_map.virtual_index_addr = 0x00;
    io_mfence();
    *apic_ioapic_map.virtual_data_addr = 0x0f000000;
    io_mfence();

    kdebug("I/O APIC ID:%#010x", ((*apic_ioapic_map.virtual_data_addr) >> 24) & 0xff);
    io_mfence();

    // 获取IO APIC Version
    *apic_ioapic_map.virtual_index_addr = 0x01;
    io_mfence();
    kdebug("IO APIC Version=%d, Max Redirection Entries=%d", *apic_ioapic_map.virtual_data_addr & 0xff, (((*apic_ioapic_map.virtual_data_addr) >> 16) & 0xff) + 1);

    // 初始化RTE表项，将所有RTE表项屏蔽
    for (int i = 0x10; i < 0x40; i += 2)
    {
        // 以0x20为起始中断向量号，初始化RTE
        apic_ioapic_write_rte(i, 0x10020 + ((i - 0x10) >> 1));
    }

    // 开启键盘中断，中断向量号为0x21，物理模式，投递至BSP处理器
    apic_ioapic_write_rte(0x12, 0x21);
    // 不需要手动启动IO APIC，只要初始化了RTE寄存器之后，io apic就会自动启用了。
    // 而且不是每台电脑都有RCBA寄存器，因此不需要手动启用IO APIC
    /*
           // get RCBA address
           io_out32(0xcf8, 0x8000f8f0);
           uint x = io_in32(0xcfc);
           uint *p;
           printk_color(RED, BLACK, "Get RCBA Address:%#010x\n", x);
           x = x & 0xffffc000UL;
           printk_color(RED, BLACK, "Get RCBA Address:%#010x\n", x);

           // get OIC address
           if (x > 0xfec00000 && x < 0xfee00000)
           {
               p = (unsigned int *)(x + 0x31feUL+SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE);
           }

           // enable IOAPIC
           x = (*p & 0xffffff00) | 0x100;
           io_mfence();
           *p = x;
           io_mfence();
           */
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

    // 映射Local APIC 寄存器地址
    mm_map_phys_addr(APIC_LOCAL_APIC_VIRT_BASE_ADDR, 0xfee00000, PAGE_2M_SIZE, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD);
}

/**
 * @brief 初始化apic控制器
 *
 */
void apic_init()
{
    // 初始化中断门， 中断使用第二个ist
    for (int i = 32; i <= 55; ++i)
        set_intr_gate(i, 2, interrupt_table[i - 32]);

    // 屏蔽类8259A芯片
    io_out8(0x21, 0xff);
    io_out8(0xa1, 0xff);
    kdebug("8259A Masked.");

    // enable IMCR
    io_out8(0x22, 0x70);
    io_out8(0x23, 0x01);

    apic_local_apic_init();

    apic_io_apic_init();
    sti();
}
/**
 * @brief 中断服务程序
 *
 * @param rsp 中断栈指针
 * @param number 中断向量号
 */
void do_IRQ(struct pt_regs *rsp, ul number)
{

    unsigned char x = io_in8(0x60);

    irq_desc_t *irq = &interrupt_desc[number - 32];

    // 执行中断上半部处理程序
    if (irq->handler != NULL)
        irq->handler(number, irq->parameter, rsp);
    else
        kwarn("Intr vector [%d] does not have a handler!");

    // 向中断控制器发送应答消息
    if (irq->controller != NULL && irq->controller->ack != NULL)
        irq->controller->ack(number);

    // 向EOI寄存器写入0x00表示结束中断
    io_mfence();
    uint *eoi = (uint *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_EOI);
    *eoi = 0x00;
    io_mfence();
}

/**
 * @brief 读取RTE寄存器
 * 由于RTE位宽为64位而IO window寄存器只有32位，因此需要两次读取
 * @param index 索引值
 * @return ul
 */
ul apic_ioapic_read_rte(unsigned char index)
{
    // 由于处理器的乱序执行的问题，需要加入内存屏障以保证结果的正确性。
    ul ret;
    // 先读取高32bit
    *apic_ioapic_map.virtual_index_addr = index + 1;
    io_mfence();

    ret = *apic_ioapic_map.virtual_data_addr;
    ret <<= 32;
    io_mfence();

    // 读取低32bit
    *apic_ioapic_map.virtual_index_addr = index;
    io_mfence();
    ret |= *apic_ioapic_map.virtual_data_addr;
    io_mfence();

    return ret;
}

/**
 * @brief 写入RTE寄存器
 *
 * @param index 索引值
 * @param value 要写入的值
 */
void apic_ioapic_write_rte(unsigned char index, ul value)
{
    // 先写入低32bit
    *apic_ioapic_map.virtual_index_addr = index;
    io_mfence();

    *apic_ioapic_map.virtual_data_addr = value & 0xffffffff;
    io_mfence();
    // 再写入高32bit
    value >>= 32;
    io_mfence();
    *apic_ioapic_map.virtual_index_addr = index + 1;
    io_mfence();
    *apic_ioapic_map.virtual_data_addr = value & 0xffffffff;
    io_mfence();
}

// =========== 中断控制操作接口 ============
void apic_ioapic_enable(ul irq_num)
{
    ul index = 0x10 + ((irq_num - 32) << 1);
    ul value = apic_ioapic_read_rte(index);
    value &= (~0x10000UL);
    apic_ioapic_write_rte(index, value);
}

void apic_ioapic_disable(ul irq_num)
{
    ul index = 0x10 + ((irq_num - 32) << 1);
    ul value = apic_ioapic_read_rte(index);
    value |= (0x10000UL);
    apic_ioapic_write_rte(index, value);
}

ul apic_ioapic_install(ul irq_num, void *arg)
{
    struct apic_IO_APIC_RTE_entry *entry = (struct apic_IO_APIC_RTE_entry *)arg;
    // RTE表项值写入对应的RTE寄存器
    apic_ioapic_write_rte(0x10 + ((irq_num - 32) << 1), *(ul *)entry);
    return 0;
}

void apic_ioapic_uninstall(ul irq_num)
{
    // 将对应的RTE表项设置为屏蔽状态
    apic_ioapic_write_rte(0x10 + ((irq_num - 32) << 1), 0x10000UL);
}

void apic_ioapic_level_ack(ul irq_num) // 电平触发
{
    // 向EOI寄存器写入0x00表示结束中断
    uint *eoi = (uint *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_EOI);
    *eoi = 0x00;
    *apic_ioapic_map.virtual_EOI_addr = irq_num;
}

void apic_ioapic_edge_ack(ul irq_num) // 边沿触发
{
    // 向EOI寄存器写入0x00表示结束中断
    uint *eoi = (uint *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_EOI);
    *eoi = 0x00;
}