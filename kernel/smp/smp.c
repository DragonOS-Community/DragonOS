#include "smp.h"
#include "../common/kprint.h"
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
}