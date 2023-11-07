#include "x86_64_ipi.h"
#include <arch/x86_64/driver/apic/apic.h>

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