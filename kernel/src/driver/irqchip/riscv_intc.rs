use alloc::{string::ToString, sync::Arc};
use system_error::SystemError;

use crate::exception::{
    handle::PerCpuDevIdIrqHandler,
    irqchip::{IrqChip, IrqChipFlags},
    irqdata::IrqData,
    irqdesc::irq_desc_manager,
    irqdomain::{irq_domain_manager, IrqDomain, IrqDomainOps},
    HardwareIrqNumber, IrqNumber,
};

static mut RISCV_INTC_DOMAIN: Option<Arc<IrqDomain>> = None;
static mut RISCV_INTC_CHIP: Option<Arc<RiscvIntcChip>> = None;

#[inline(always)]
pub fn riscv_intc_domain() -> &'static Option<Arc<IrqDomain>> {
    unsafe { &RISCV_INTC_DOMAIN }
}

#[inline(always)]
fn riscv_intc_chip() -> Option<&'static Arc<RiscvIntcChip>> {
    unsafe { RISCV_INTC_CHIP.as_ref() }
}

#[derive(Debug)]
struct RiscvIntcChip;

impl IrqChip for RiscvIntcChip {
    fn name(&self) -> &'static str {
        "RISC-V INTC"
    }

    fn irq_disable(&self, _irq: &Arc<IrqData>) {}

    fn irq_mask(&self, irq: &Arc<IrqData>) -> Result<(), SystemError> {
        unsafe { riscv::register::sie::clear_bits(1 << irq.hardware_irq().data()) };
        Ok(())
    }

    fn irq_unmask(&self, irq: &Arc<IrqData>) -> Result<(), SystemError> {
        unsafe { riscv::register::sie::set_bits(1 << irq.hardware_irq().data()) };
        Ok(())
    }

    fn irq_ack(&self, irq: &Arc<IrqData>) {
        todo!()
    }

    fn can_mask_ack(&self) -> bool {
        false
    }

    fn irq_eoi(&self, _irq: &Arc<IrqData>) {
        /*
         * The RISC-V INTC driver uses handle_percpu_devid_irq() flow
         * for the per-HART local interrupts and child irqchip drivers
         * (such as PLIC, SBI IPI, CLINT, APLIC, IMSIC, etc) implement
         * chained handlers for the per-HART local interrupts.
         *
         * In the absence of irq_eoi(), the chained_irq_enter() and
         * chained_irq_exit() functions (used by child irqchip drivers)
         * will do unnecessary mask/unmask of per-HART local interrupts
         * at the time of handling interrupts. To avoid this, we provide
         * an empty irq_eoi() callback for RISC-V INTC irqchip.
         */
    }

    fn can_set_affinity(&self) -> bool {
        false
    }

    fn can_set_flow_type(&self) -> bool {
        false
    }

    fn flags(&self) -> IrqChipFlags {
        todo!()
    }
}

#[derive(Debug)]
struct RiscvIntcDomainOps;

impl IrqDomainOps for RiscvIntcDomainOps {
    fn map(
        &self,
        irq_domain: &Arc<IrqDomain>,
        hwirq: HardwareIrqNumber,
        virq: IrqNumber,
    ) -> Result<(), SystemError> {
        irq_desc_manager().set_percpu_devid_all(virq)?;
        irq_domain_manager().domain_set_info(
            irq_domain,
            virq,
            hwirq,
            riscv_intc_chip().unwrap().clone() as Arc<dyn IrqChip>,
            irq_domain.host_data(),
            &PerCpuDevIdIrqHandler,
            None,
            None,
        );

        return Ok(());
    }

    fn unmap(&self, irq_domain: &Arc<IrqDomain>, virq: IrqNumber) {
        todo!("riscv_intc_domain_ops::unmap");
    }
}

#[inline(never)]
pub unsafe fn riscv_intc_init() -> Result<(), SystemError> {
    let intc_chip = Arc::new(RiscvIntcChip);

    unsafe {
        RISCV_INTC_CHIP = Some(intc_chip);
    }

    let intc_domain = irq_domain_manager()
        .create_and_add_linear("riscv-intc".to_string(), &RiscvIntcDomainOps, 64)
        .ok_or_else(|| {
            kerror!("Failed to create riscv-intc domain");
            SystemError::ENXIO
        })?;

    irq_domain_manager().set_default_domain(intc_domain.clone());

    unsafe {
        RISCV_INTC_DOMAIN = Some(intc_domain);
    }

    return Ok(());
}
