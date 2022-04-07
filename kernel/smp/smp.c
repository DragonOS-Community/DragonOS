#include "smp.h"
#include "../common/kprint.h"
#include "../driver/interrupt/apic/apic.h"
#include "../exception/gate.h"
#include "../common/cpu.h"
#include "../mm/slab.h"
#include "../process/process.h"
#include "../process/spinlock.h"

static spinlock_t multi_core_starting_lock; // 多核启动锁

static struct acpi_Processor_Local_APIC_Structure_t *proc_local_apic_structs[MAX_SUPPORTED_PROCESSOR_NUM];
static uint32_t total_processor_num = 0;
int current_starting_cpu = 0;



void smp_init()
{
    spin_init(&multi_core_starting_lock);

    ul tmp_vaddr[MAX_SUPPORTED_PROCESSOR_NUM] = {0};
    
    apic_get_ics(ACPI_ICS_TYPE_PROCESSOR_LOCAL_APIC, tmp_vaddr, &total_processor_num);

    kdebug("processor num=%d", total_processor_num);
    for (int i = 0; i < total_processor_num; ++i)
        proc_local_apic_structs[i] = (struct acpi_Processor_Local_APIC_Structure_t *)(tmp_vaddr[i]);

    //*(uchar *)0x20000 = 0xf4; // 在内存的0x20000处写入HLT指令(AP处理器会执行物理地址0x20000的代码)
    // 将引导程序复制到物理地址0x20000处
    memcpy((unsigned char *)0x20000, _apu_boot_start, (unsigned long)&_apu_boot_end - (unsigned long)&_apu_boot_start);

    // wrmsr(0x830, 0xc4500); // init IPI
    struct INT_CMD_REG icr_entry;
    icr_entry.dest_mode = DEST_PHYSICAL;
    icr_entry.deliver_status = IDLE;
    icr_entry.res_1 = 0;
    icr_entry.level = ICR_LEVEL_DE_ASSERT;
    icr_entry.trigger = EDGE_TRIGGER;
    icr_entry.res_2 = 0;
    icr_entry.res_3 = 0;

    icr_entry.vector = 0x00;
    icr_entry.deliver_mode = ICR_INIT;
    icr_entry.dest_shorthand = ICR_ALL_EXCLUDE_Self;
    icr_entry.destination.x2apic_destination = 0x00;

    wrmsr(0x830, *(unsigned long *)&icr_entry); // INIT IPI

    for (int i = 1; i < total_processor_num; ++i) // i从1开始，不初始化bsp
    {
        spin_lock(&multi_core_starting_lock);
        current_starting_cpu = i;

        kdebug("[core %d] acpi processor UID=%d, APIC ID=%d, flags=%#010lx", i, proc_local_apic_structs[i]->ACPI_Processor_UID, proc_local_apic_structs[i]->ACPI_ID, proc_local_apic_structs[i]->flags);
        // 为每个AP处理器分配栈空间、tss空间
        cpu_core_info[i].stack_start = (uint64_t)kmalloc(STACK_SIZE, 0);
        kdebug("cpu_core_info[i].stack_start =%#018lx", (uint64_t)kmalloc(STACK_SIZE, 0));
        cpu_core_info[i].stack_start += STACK_SIZE;
        kdebug("cpu_core_info[i].stack_base =%#018lx", (uint64_t)kmalloc(STACK_SIZE, 0));

        cpu_core_info[i].tss_vaddr = (uint64_t)kmalloc(128, 0);

        set_tss_descriptor(10 + (i * 2), (void *)(cpu_core_info[i].tss_vaddr));
        set_tss64((uint *)cpu_core_info[i].tss_vaddr, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start, cpu_core_info[i].stack_start);
        kdebug("GDT Table %#018lx, \t %#018lx", GDT_Table[10 + i * 2], GDT_Table[10 + i * 2 + 1]);
        kdebug("(cpu_core_info[i].tss_vaddr)=%#018lx", (cpu_core_info[i].tss_vaddr));
        kdebug("(cpu_core_info[i].stack_start)=%#018lx", (cpu_core_info[i].stack_start));
        icr_entry.vector = 0x20;
        icr_entry.deliver_mode = ICR_Start_up;
        icr_entry.dest_shorthand = ICR_No_Shorthand;
        icr_entry.destination.x2apic_destination = proc_local_apic_structs[i]->ACPI_ID;

        // 先init ipi， 然后连续发送两次start-up IPI
        // x2APIC下，ICR寄存器地址为0x830
        // xAPIC下则为0xfee00300(31-0) 0xfee00310 (63-32)
        
        wrmsr(0x830, *(ul *)&icr_entry); // start-up IPI
        wrmsr(0x830, *(ul *)&icr_entry); // start-up IPI
        //for(int k=0;i<1e5;++k);
    }
}

/**
 * @brief AP处理器启动后执行的第一个函数
 *
 */
void smp_ap_start()
{
    // 切换栈基地址
    //uint64_t stack_start = (uint64_t)kmalloc(STACK_SIZE, 0) + STACK_SIZE;
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
    kdebug("current=%d", current_starting_cpu);
    apic_init_ap_core_local_apic();

    load_TR(10 + current_starting_cpu * 2);
    spin_unlock(&multi_core_starting_lock);
    sti();
    kdebug("IDT_addr = %#018lx", &IDT_Table);
    //sti();
    // int a = 1 / 0; // 在这儿会出现异常，cs fs gs ss寄存器会被改变
    kdebug("IDT_addr = %#018lx", &IDT_Table);
    hlt();
}