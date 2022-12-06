#include "apic.h"
#include <common/kprint.h>
#include <common/printk.h>
#include <common/cpu.h>
#include <common/glib.h>
#include <exception/gate.h>
#include <driver/acpi/acpi.h>

#include <exception/softirq.h>
#include <process/process.h>
#include <sched/sched.h>

#pragma GCC push_options
#pragma GCC optimize("O0")
// 导出定义在irq.c中的中段门表
extern void (*interrupt_table[24])(void);

static bool flag_support_apic = false;
static bool flag_support_x2apic = false;
uint8_t __apic_enable_state = APIC_XAPIC_ENABLED;
static uint local_apic_version;
static uint local_apic_max_LVT_entries;

static struct acpi_Multiple_APIC_Description_Table_t *madt;
static struct acpi_IO_APIC_Structure_t *io_apic_ICS;

static void __local_apic_xapic_init();
static void __local_apic_x2apic_init();

static __always_inline void __send_eoi()
{
    if (CURRENT_APIC_STATE == APIC_X2APIC_ENABLED)
    {
        __asm__ __volatile__("movq	$0x00,	%%rdx	\n\t"
                             "movq	$0x00,	%%rax	\n\t"
                             "movq 	$0x80b,	%%rcx	\n\t"
                             "wrmsr	\n\t" ::
                                 : "memory");
    }
    else
    {

        io_mfence();
        __write4b(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_EOI, 0);
        io_mfence();
    }
}

/**
 * @brief 初始化io_apic
 *
 */
void apic_io_apic_init()
{

    ul madt_addr;
    acpi_iter_SDT(acpi_get_MADT, &madt_addr);
    madt = (struct acpi_Multiple_APIC_Description_Table_t *)madt_addr;

    // kdebug("MADT->local intr controller addr=%#018lx", madt->Local_Interrupt_Controller_Address);
    // kdebug("MADT->length= %d bytes", madt->header.Length);
    //  寻找io apic的ICS
    void *ent = (void *)(madt_addr) + sizeof(struct acpi_Multiple_APIC_Description_Table_t);
    struct apic_Interrupt_Controller_Structure_header_t *header = (struct apic_Interrupt_Controller_Structure_header_t *)ent;
    while (header->length > 2)
    {
        header = (struct apic_Interrupt_Controller_Structure_header_t *)ent;
        if (header->type == 1)
        {
            struct acpi_IO_APIC_Structure_t *t = (struct acpi_IO_APIC_Structure_t *)ent;
            // kdebug("IO apic addr = %#018lx", t->IO_APIC_Address);
            io_apic_ICS = t;
            break;
        }

        ent += header->length;
    }
    // kdebug("Global_System_Interrupt_Base=%d", io_apic_ICS->Global_System_Interrupt_Base);

    apic_ioapic_map.addr_phys = io_apic_ICS->IO_APIC_Address;
    apic_ioapic_map.virtual_index_addr = (unsigned char *)APIC_IO_APIC_VIRT_BASE_ADDR;
    apic_ioapic_map.virtual_data_addr = (uint *)(APIC_IO_APIC_VIRT_BASE_ADDR + 0x10);
    apic_ioapic_map.virtual_EOI_addr = (uint *)(APIC_IO_APIC_VIRT_BASE_ADDR + 0x40);

    // kdebug("(ul)apic_ioapic_map.virtual_index_addr=%#018lx", (ul)apic_ioapic_map.virtual_index_addr);
    // 填写页表，完成地址映射
    mm_map_phys_addr((ul)apic_ioapic_map.virtual_index_addr, apic_ioapic_map.addr_phys, PAGE_2M_SIZE, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, false);

    // 设置IO APIC ID 为0x0f000000
    *apic_ioapic_map.virtual_index_addr = 0x00;
    io_mfence();
    *apic_ioapic_map.virtual_data_addr = 0x0f000000;
    io_mfence();

    // kdebug("I/O APIC ID:%#010x", ((*apic_ioapic_map.virtual_data_addr) >> 24) & 0xff);
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

    // 不需要手动启动IO APIC，只要初始化了RTE寄存器之后，io apic就会自动启用了。
    // 而且不是每台电脑都有RCBA寄存器，因此不需要手动启用IO APIC
}

/**
 * @brief 初始化AP处理器的Local apic
 *
 */
void apic_init_ap_core_local_apic()
{
    kinfo("Initializing AP-core's local apic...");
    uint eax, edx;
    // 启用xAPIC 和x2APIC
    uint64_t ia32_apic_base = rdmsr(0x1b);
    ia32_apic_base |= (1 << 11);
    if (flag_support_x2apic) // 如果支持x2apic，则启用
    {
        ia32_apic_base |= (1 << 10);
        wrmsr(0x1b, ia32_apic_base);
    }
    ia32_apic_base = rdmsr(0x1b);
    eax = ia32_apic_base & 0xffffffff;

    // 检测是否成功启用xAPIC和x2APIC
    if ((eax & 0xc00) == 0xc00)
        kinfo("xAPIC & x2APIC enabled!");
    else if ((eax & 0x800) == 0x800)
        kinfo("Only xAPIC enabled!");
    else
        kerror("Both xAPIC and x2APIC are not enabled.");

    // 设置SVR寄存器，开启local APIC、禁止EOI广播
    if (flag_support_x2apic) // 当前为x2APIC
        __local_apic_x2apic_init();
    else // 当前为xapic
        __local_apic_xapic_init();
}

/**
 * @brief 当前使用xapic来初始化local apic
 *
 */
static void __local_apic_xapic_init()
{
    __apic_enable_state = APIC_XAPIC_ENABLED;
    // 设置svr的 apic软件使能位
    uint64_t qword = *(uint64_t *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_SVR);

    qword |= (1 << 8);
    *(uint64_t *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_SVR) = qword;
    qword = *(uint64_t *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_SVR);
    if (qword & 0x100)
        kinfo("APIC Software Enabled.");
    if (qword & 0x1000)
        kinfo("EOI-Broadcast Suppression Enabled.");

    // 从  Local APIC Version register 获取Local APIC Version
    qword = *(uint64_t *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_Version);
    qword &= 0xffffffff;

    local_apic_max_LVT_entries = ((qword >> 16) & 0xff) + 1;
    local_apic_version = qword & 0xff;

    kdebug("local APIC Version:%#010x,Max LVT Entry:%#010x,SVR(Suppress EOI Broadcast):%#04x\t", local_apic_version, local_apic_max_LVT_entries, (qword >> 24) & 0x1);

    if ((qword & 0xff) < 0x10)
    {
        kdebug("82489DX discrete APIC");
    }
    else if (((qword & 0xff) >= 0x10) && ((qword & 0xff) <= 0x15))
        kdebug("Integrated APIC.");

    io_mfence();
    // 如果写入这里的话，在有的机器上面会报错
    // *(uint *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_LVT_CMCI) = APIC_LVT_INT_MASKED;
    io_mfence();
    *(uint *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_LVT_TIMER) = APIC_LVT_INT_MASKED;
    io_mfence();

    *(uint *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_LVT_THERMAL) = APIC_LVT_INT_MASKED;
    io_mfence();
    *(uint *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_LVT_PERFORMANCE_MONITOR) = APIC_LVT_INT_MASKED;
    io_mfence();
    *(uint *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_LVT_LINT0) = APIC_LVT_INT_MASKED;
    io_mfence();
    *(uint *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_LVT_LINT1) = APIC_LVT_INT_MASKED;
    io_mfence();
    *(uint *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_LVT_ERROR) = APIC_LVT_INT_MASKED;
    io_mfence();

    kdebug("All LVT Masked");
}

/**
 * @brief 当前使用x2apic来初始化local apic
 *
 */
static void __local_apic_x2apic_init()
{
    __apic_enable_state = APIC_X2APIC_ENABLED;
    uint32_t eax, edx;
    __asm__ __volatile__("movq $0x80f, %%rcx    \n\t"
                         "rdmsr  \n\t"
                         "bts $8, %%rax  \n\t"
                         //                         "bts $12, %%rax \n\t"
                         "movq $0x80f, %%rcx    \n\t"
                         "wrmsr  \n\t"
                         "movq $0x80f , %%rcx   \n\t"
                         "rdmsr \n\t"
                         : "=a"(eax), "=d"(edx)::"memory");
    if (eax & 0x100)
        kinfo("APIC Software Enabled.");
    if (eax & 0x1000)
        kinfo("EOI-Broadcast Suppression Enabled.");

    // 获取Local APIC Version
    // 0x803处是 Local APIC Version register
    __asm__ __volatile__("movq $0x803, %%rcx    \n\t"
                         "rdmsr  \n\t"
                         : "=a"(eax), "=d"(edx)::"memory");

    local_apic_max_LVT_entries = ((eax >> 16) & 0xff) + 1;
    local_apic_version = eax & 0xff;

    kdebug("local APIC Version:%#010x,Max LVT Entry:%#010x,SVR(Suppress EOI Broadcast):%#04x\t", local_apic_version, local_apic_max_LVT_entries, (eax >> 24) & 0x1);

    if ((eax & 0xff) < 0x10)
        kdebug("82489DX discrete APIC");
    else if (((eax & 0xff) >= 0x10) && ((eax & 0xff) <= 0x15))
        kdebug("Integrated APIC.");

    // 由于尚未配置LVT对应的处理程序，因此先屏蔽所有的LVT
    __asm__ __volatile__(             // "movq 	$0x82f,	%%rcx	\n\t" // CMCI
                                      // "wrmsr	\n\t"
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
}

/**
 * @brief 初始化local apic
 *
 */
void apic_local_apic_init()
{
    uint64_t ia32_apic_base = rdmsr(0x1b);
    // kdebug("apic base=%#018lx", (ia32_apic_base & 0x1FFFFFFFFFF000));
    // 映射Local APIC 寄存器地址
    mm_map_phys_addr(APIC_LOCAL_APIC_VIRT_BASE_ADDR, (ia32_apic_base & 0x1FFFFFFFFFFFFF), PAGE_2M_SIZE, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, false);
    uint a, b, c, d;

    cpu_cpuid(1, 0, &a, &b, &c, &d);

    // kdebug("CPUID 0x01, eax:%#010lx, ebx:%#010lx, ecx:%#010lx, edx:%#010lx", a, b, c, d);

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
        flag_support_x2apic = false;
        kwarn("This computer does not support x2APIC");
    }

    uint eax, edx;
    // 启用xAPIC 和x2APIC
    ia32_apic_base = rdmsr(0x1b);
    ia32_apic_base |= (1 << 11);
    if (flag_support_x2apic) // 如果支持x2apic，则启用
    {
        ia32_apic_base |= (1 << 10);
        wrmsr(0x1b, ia32_apic_base);
    }
    ia32_apic_base = rdmsr(0x1b);
    eax = ia32_apic_base & 0xffffffff;

    // 检测是否成功启用xAPIC和x2APIC
    if ((eax & 0xc00) == 0xc00)
        kinfo("xAPIC & x2APIC enabled!");
    else if ((eax & 0x800) == 0x800)
        kinfo("Only xAPIC enabled!");
    else
        kerror("Both xAPIC and x2APIC are not enabled.");

    // 设置SVR寄存器，开启local APIC、禁止EOI广播
    if (flag_support_x2apic) // 当前为x2APIC
        __local_apic_x2apic_init();
    else // 当前为xapic
        __local_apic_xapic_init();

    // 获取Local APIC的基础信息 （参见英特尔开发手册Vol3A 10-39）
    //                          Table 10-6. Local APIC Register Address Map Supported by x2APIC
    // 获取 Local APIC ID
    // 0x802处是x2APIC ID 位宽32bits 的 Local APIC ID register
    /*
    __asm__ __volatile__("movq $0x802, %%rcx    \n\t"
                         "rdmsr  \n\t"
                         : "=a"(eax), "=d"(edx)::"memory");
    */
    // kdebug("get Local APIC ID: edx=%#010x, eax=%#010x", edx, eax);
    // kdebug("local_apic_id=%#018lx", *(uint *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_ID));
}

/**
 * @brief 初始化apic控制器
 *
 */
int apic_init()
{
    // 初始化中断门， 中断使用rsp0防止在软中断时发生嵌套，然后处理器重新加载导致数据被抹掉
    for (int i = 32; i <= 55; ++i)
        set_intr_gate(i, 0, interrupt_table[i - 32]);

    // 设置local apic中断门
    for (int i = 150; i < 160; ++i)
        set_intr_gate(i, 0, local_apic_interrupt_table[i - 150]);

    //  屏蔽类8259A芯片
    io_out8(0x21, 0xff);

    io_out8(0xa1, 0xff);

    // 写入8259A pic的EOI位
    io_out8(0x20, 0x20);
    io_out8(0xa0, 0x20);

    kdebug("8259A Masked.");

    // enable IMCR
    io_out8(0x22, 0x70);
    io_out8(0x23, 0x01);

    apic_local_apic_init();

    apic_io_apic_init();

    // get RCBA address
    io_out32(0xcf8, 0x8000f8f0);
    uint32_t RCBA_phys = io_in32(0xcfc);

    // 获取RCBA寄存器的地址
    if (RCBA_phys > 0xfec00000 && RCBA_phys < 0xfee00000)
        RCBA_vaddr = SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + RCBA_phys;
    else
    {
        RCBA_vaddr = 0;
        kwarn("Cannot get RCBA address. RCBA_phys=%#010lx", RCBA_phys);
    }
    sti();
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
        if (irq->controller != NULL && irq->controller->ack != NULL)
            irq->controller->ack(number);
        else
            __send_eoi();
    }
    else if (number >= 200)
    {
        apic_local_apic_edge_ack(number);

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
        if (irq->controller != NULL && irq->controller->ack != NULL)
            irq->controller->ack(number);
        else
            __send_eoi(); // 向EOI寄存器写入0x00表示结束中断
    }
    else
    {

        kwarn("do IRQ receive: %d", number);
        // 忽略未知中断
        return;
    }

    // kdebug("before softirq");
    // 进入软中断处理程序
    do_softirq();

    // kdebug("after softirq");
    // 检测当前进程是否持有自旋锁，若持有自旋锁，则不进行抢占式的进程调度
    if (current_pcb->preempt_count > 0)
        return;
    else if (current_pcb->preempt_count < 0)
        kBUG("current_pcb->preempt_count<0! pid=%d", current_pcb->pid); // should not be here

    // 检测当前进程是否可被调度
    if (current_pcb->flags & PF_NEED_SCHED)
    {
        io_mfence();
        sched();
    }
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
    __send_eoi();
    *apic_ioapic_map.virtual_EOI_addr = irq_num;
}

void apic_ioapic_edge_ack(ul irq_num) // 边沿触发
{

    // 向EOI寄存器写入0x00表示结束中断
    /*
        uint *eoi = (uint *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_EOI);
        *eoi = 0x00;

        */
    __send_eoi();
}

/**
 * @brief local apic 边沿触发应答
 *
 * @param irq_num
 */

void apic_local_apic_edge_ack(ul irq_num)
{
    // 向EOI寄存器写入0x00表示结束中断
    __send_eoi();
}

/**
 * @brief 读取指定类型的 Interrupt Control Structure
 *
 * @param type ics的类型
 * @param ret_vaddr 对应的ICS的虚拟地址数组
 * @param total 返回数组的元素总个数
 * @return uint
 */
uint apic_get_ics(const uint type, ul ret_vaddr[], uint *total)
{
    void *ent = (void *)(madt) + sizeof(struct acpi_Multiple_APIC_Description_Table_t);
    struct apic_Interrupt_Controller_Structure_header_t *header = (struct apic_Interrupt_Controller_Structure_header_t *)ent;
    bool flag = false;

    uint cnt = 0;

    while (header->length > 2)
    {
        header = (struct apic_Interrupt_Controller_Structure_header_t *)ent;
        if (header->type == type)
        {
            ret_vaddr[cnt++] = (ul)ent;
            flag = true;
        }
        ent += header->length;
    }

    *total = cnt;
    if (!flag)
        return APIC_E_NOTFOUND;
    else
        return APIC_SUCCESS;
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
                         uint8_t deliver_status, uint8_t polarity, uint8_t irr, uint8_t trigger, uint8_t mask, uint8_t dest_apicID)
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