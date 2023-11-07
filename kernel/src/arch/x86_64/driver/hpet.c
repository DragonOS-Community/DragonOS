#include <common/glib.h>
#include <common/kprint.h>
#include <arch/x86_64/driver/apic/apic.h>

extern void rs_handle_hpet_irq(uint32_t timer_num);

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
    rs_handle_hpet_irq(param);
}

void c_hpet_register_irq()
{
    struct apic_IO_APIC_RTE_entry entry;
    apic_make_rte_entry(&entry, 34, IO_APIC_FIXED, DEST_PHYSICAL, IDLE, POLARITY_HIGH, IRR_RESET, EDGE_TRIGGER, MASKED, 0);
    irq_register(34, &entry, &HPET_handler, 0, &HPET_intr_controller, "HPET0");
}
