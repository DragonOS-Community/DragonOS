#include "x86_64_ipi.h"
#include <driver/interrupt/apic/apic.h>

void ipi_send_IPI(uint32_t dest_mode, uint32_t deliver_status, uint32_t level, uint32_t trigger,
                  uint32_t vector, uint32_t deliver_mode, uint32_t dest_shorthand, uint32_t destination)
{
    struct INT_CMD_REG icr_entry;
    icr_entry.dest_mode = dest_mode;
    icr_entry.deliver_status = deliver_status;
    icr_entry.res_1 = 0;
    icr_entry.level = level;
    icr_entry.trigger = trigger;
    icr_entry.res_2 = 0;
    icr_entry.res_3 = 0;

    icr_entry.vector = vector;
    icr_entry.deliver_mode = deliver_mode;
    icr_entry.dest_shorthand = dest_shorthand;

    // x2APIC下，ICR寄存器地址为0x830
    // xAPIC下则为0xfee00300(31-0) 0xfee00310 (63-32)
    if (CURRENT_APIC_STATE == APIC_X2APIC_ENABLED) // x2APIC
    {
        icr_entry.destination.x2apic_destination = destination;
        wrmsr(0x830, *(unsigned long *)&icr_entry); // 发送ipi
    }
    else // xAPIC
    {

        icr_entry.destination.apic_destination.dest_field = destination & 0xff;
        icr_entry.destination.apic_destination.res_4 = 0;
        // 先向高32bit写数据，然后再向低32bit写数据，不能调转
        *(uint32_t *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + 0x310) = (uint32_t)(((*(ul *)&icr_entry) >> 32) & 0xffffffff);
        *(uint32_t *)(APIC_LOCAL_APIC_VIRT_BASE_ADDR + 0x300) = (uint32_t)((*(ul *)&icr_entry) & 0xffffffff);
    }
}

int ipi_regiserIPI(uint64_t irq_num, void *arg,
                   void (*handler)(uint64_t irq_num, uint64_t param, struct pt_regs *regs),
                   uint64_t param, hardware_intr_controller *controller, char *irq_name)
{
    irq_desc_t *p = &SMP_IPI_desc[irq_num - 200];
    p->controller = NULL; // 由于ipi不涉及到具体的硬件操作，因此不需要controller
    p->irq_name = irq_name;
    p->parameter = param;
    p->flags = 0;
    p->handler = handler;
    return 0;
}