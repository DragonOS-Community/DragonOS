#include "smp.h"
#include "../common/kprint.h"
#include "../driver/interrupt/apic/apic.h"

extern void apic_local_apic_init();

static struct acpi_Processor_Local_APIC_Structure_t *proc_local_apic_structs[MAX_SUPPORTED_PROCESSOR_NUM];
static uint32_t total_processor_num = 0;

void smp_init()
{
    ul tmp_vaddr[MAX_SUPPORTED_PROCESSOR_NUM] = {0};

    apic_get_ics(ACPI_ICS_TYPE_PROCESSOR_LOCAL_APIC, tmp_vaddr, &total_processor_num);

    kdebug("processor num=%d", total_processor_num);
    for (int i = 0; i < total_processor_num; ++i)
        proc_local_apic_structs[i] = (struct acpi_Processor_Local_APIC_Structure_t *)(tmp_vaddr[i]);

    for (int i = 0; i < total_processor_num; ++i)
    {
        kdebug("[core %d] acpi processor UID=%d, APIC ID=%d, flags=%#010lx", i, proc_local_apic_structs[i]->ACPI_Processor_UID, proc_local_apic_structs[i]->ACPI_ID, proc_local_apic_structs[i]->flags);
    }

    //*(uchar *)0x20000 = 0xf4; // 在内存的0x20000处写入HLT指令(AP处理器会执行物理地址0x20000的代码)
    // 将引导程序复制到物理地址0x20000处
    memcpy((unsigned char *)0x20000, _apu_boot_start, (unsigned long)&_apu_boot_end - (unsigned long)&_apu_boot_start);

    // 先init ipi， 然后连续发送两次start-up IPI
    // x2APIC下，ICR寄存器地址为0x830
    // xAPIC下则为0xfee00300(31-0) 0xfee00310 (63-32)
    wrmsr(0x830, 0xc4500);  // init IPI
    wrmsr(0x830, 0xc4620);  // start-up IPI
    wrmsr(0x830, 0xc4620);  // start-up IPI
}

/**
 * @brief AP处理器启动后执行的第一个函数
 * 
 */
void smp_ap_start()
{
    ksuccess("AP core successfully started!");
    kinfo("Initializing AP's local apic...");
    apic_local_apic_init();
    while(1);
}