#include "smp.h"
#include "../common/kprint.h"
#include "../driver/interrupt/apic/apic.h"
#include "../exception/gate.h"
#include "../common/cpu.h"
#include "../mm/slab.h"
#include "../process/process.h"
#include "../process/spinlock.h"

#include "ipi.h"

static spinlock_t multi_core_starting_lock; // 多核启动锁

static struct acpi_Processor_Local_APIC_Structure_t *proc_local_apic_structs[MAX_SUPPORTED_PROCESSOR_NUM];
static uint32_t total_processor_num = 0;
int current_starting_cpu = 0;

int num_cpu_started = 1;

void smp_init()
{
    spin_init(&multi_core_starting_lock); // 初始化多核启动锁
    ul tmp_vaddr[MAX_SUPPORTED_PROCESSOR_NUM] = {0};

    apic_get_ics(ACPI_ICS_TYPE_PROCESSOR_LOCAL_APIC, tmp_vaddr, &total_processor_num);

    kdebug("processor num=%d", total_processor_num);
    for (int i = 0; i < total_processor_num; ++i)
        proc_local_apic_structs[i] = (struct acpi_Processor_Local_APIC_Structure_t *)(tmp_vaddr[i]);

    //*(uchar *)0x20000 = 0xf4; // 在内存的0x20000处写入HLT指令(AP处理器会执行物理地址0x20000的代码)
    // 将引导程序复制到物理地址0x20000处
    memcpy((unsigned char *)phys_2_virt(0x20000), _apu_boot_start, (unsigned long)&_apu_boot_end - (unsigned long)&_apu_boot_start);
    
    // 设置多核IPI中断门
    for (int i = 200; i < 210; ++i)
        set_intr_gate(i, 2, SMP_interrupt_table[i - 200]);

    memset((void *)SMP_IPI_desc, 0, sizeof(irq_desc_t) * SMP_IRQ_NUM);

    ipi_send_IPI(DEST_PHYSICAL, IDLE, ICR_LEVEL_DE_ASSERT, EDGE_TRIGGER, 0x00, ICR_INIT, ICR_ALL_EXCLUDE_Self, true, 0x00);

    for (int i = 1; i < total_processor_num; ++i) // i从1开始，不初始化bsp
    {
        if (proc_local_apic_structs[i]->ACPI_Processor_UID == 0)
            --total_processor_num;
        spin_lock(&multi_core_starting_lock);
        current_starting_cpu = i;

        kdebug("[core %d] acpi processor UID=%d, APIC ID=%d, flags=%#010lx", i, proc_local_apic_structs[i]->ACPI_Processor_UID, proc_local_apic_structs[i]->ACPI_ID, proc_local_apic_structs[i]->flags);
        // 为每个AP处理器分配栈空间、tss空间
        cpu_core_info[i].stack_start = (uint64_t)kmalloc(STACK_SIZE, 0) + STACK_SIZE;

        cpu_core_info[i].tss_vaddr = (uint64_t)kmalloc(128, 0);

        set_tss_descriptor(10 + (i * 2), (void *)virt_2_phys(cpu_core_info[i].tss_vaddr));

        set_tss64((uint *)cpu_core_info[i].tss_vaddr, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start);
        kdebug("phys_2_virt(GDT_Table)=%#018lx",phys_2_virt(GDT_Table));
        kdebug("GDT Table %#018lx, \t %#018lx", *(ul *)(phys_2_virt(GDT_Table) + 10 + i * 2), *(ul *)(phys_2_virt(GDT_Table) + 10 + i * 2 + 1));
        // kdebug("(cpu_core_info[i].tss_vaddr)=%#018lx", (cpu_core_info[i].tss_vaddr));
        kdebug("(cpu_core_info[i].stack_start)=%#018lx", (cpu_core_info[i].stack_start));
        // 连续发送两次start-up IPI
        ipi_send_IPI(DEST_PHYSICAL, IDLE, ICR_LEVEL_DE_ASSERT, EDGE_TRIGGER, 0x20, ICR_Start_up, ICR_No_Shorthand, true, proc_local_apic_structs[i]->ACPI_ID);
        ipi_send_IPI(DEST_PHYSICAL, IDLE, ICR_LEVEL_DE_ASSERT, EDGE_TRIGGER, 0x20, ICR_Start_up, ICR_No_Shorthand, true, proc_local_apic_structs[i]->ACPI_ID);
    }

    while (num_cpu_started != total_processor_num)
        __asm__ __volatile__("pause" ::
                                 : "memory");

    kinfo("Cleaning page table remapping...\n");
    
    // 由于ap处理器初始化过程需要用到0x00处的地址，因此初始化完毕后才取消内存地址的重映射
    //todo: 取消低0-2M的地址映射
    for (int i = 1; i < 128; ++i)
    {

        *(ul *)(phys_2_virt(global_CR3) + i) = 0UL;
    }

    kinfo("Successfully cleaned page table remapping!\n");
   
}

/**
 * @brief AP处理器启动后执行的第一个函数
 *
 */
void smp_ap_start()
{

    //  切换栈基地址
    //  uint64_t stack_start = (uint64_t)kmalloc(STACK_SIZE, 0) + STACK_SIZE;
    __asm__ __volatile__("movq %0, %%rbp \n\t" ::"m"(cpu_core_info[current_starting_cpu].stack_start)
                         : "memory");
    __asm__ __volatile__("movq %0, %%rsp \n\t" ::"m"(cpu_core_info[current_starting_cpu].stack_start)
                         : "memory");
    /*
        __asm__ __volatile__("movq %0, %%rbp \n\t" ::"m"(stack_start)
                             : "memory");
        __asm__ __volatile__("movq %0, %%rsp \n\t" ::"m"(stack_start)
                             : "memory");*/
    ksuccess("AP core successfully started!");

    ++num_cpu_started;

    kdebug("current cpu = %d", current_starting_cpu);

    apic_init_ap_core_local_apic();
    load_TR(10 + current_starting_cpu * 2);

    sti();
    kdebug("IDT_addr = %#018lx", phys_2_virt(IDT_Table));

    spin_unlock(&multi_core_starting_lock);
    while (1) // 这里要循环hlt，原因是当收到中断后，核心会被唤醒，处理完中断之后不会自动hlt
        hlt();
}