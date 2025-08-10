use alloc::{string::ToString, sync::Arc};
use log::error;
use system_error::SystemError;

use crate::{
    arch::interrupt::TrapFrame,
    driver::clocksource::timer_riscv::{riscv_sbi_timer_irq_desc_init, RiscVSbiTimer},
    exception::{
        handle::PerCpuDevIdIrqHandler,
        irqchip::{IrqChip, IrqChipFlags},
        irqdata::IrqData,
        irqdesc::{irq_desc_manager, GenericIrqHandler},
        irqdomain::{irq_domain_manager, IrqDomain, IrqDomainOps},
        softirq::do_softirq,
        HardwareIrqNumber, IrqNumber,
    },
    libs::spinlock::{SpinLock, SpinLockGuard},
    sched::{SchedMode, __schedule},
};

use super::riscv_sifive_plic::do_plic_irq;

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

/// RISC-V INTC虚拟中断号的起始值（192映射物理的0）
pub const RISCV_INTC_VIRQ_START: u32 = 192;

#[derive(Debug)]
struct RiscvIntcChip {
    inner: SpinLock<InnerIrqChip>,
}

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

    fn irq_ack(&self, _irq: &Arc<IrqData>) {}

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
        self.inner().flags
    }
}

impl RiscvIntcChip {
    const IRQ_SIZE: u32 = 64;
    fn new() -> Self {
        Self {
            inner: SpinLock::new(InnerIrqChip {
                flags: IrqChipFlags::empty(),
            }),
        }
    }
    fn inner(&self) -> SpinLockGuard<InnerIrqChip> {
        self.inner.lock_irqsave()
    }
}

#[derive(Debug)]
struct InnerIrqChip {
    flags: IrqChipFlags,
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

    fn unmap(&self, _irq_domain: &Arc<IrqDomain>, _virq: IrqNumber) {
        todo!("riscv_intc_domain_ops::unmap");
    }
}

#[inline(never)]
pub unsafe fn riscv_intc_init() -> Result<(), SystemError> {
    let intc_chip = Arc::new(RiscvIntcChip::new());

    unsafe {
        RISCV_INTC_CHIP = Some(intc_chip);
    }

    let intc_domain = irq_domain_manager()
        .create_and_add_linear(
            "riscv-intc".to_string(),
            &RiscvIntcDomainOps,
            RiscvIntcChip::IRQ_SIZE,
        )
        .ok_or_else(|| {
            error!("Failed to create riscv-intc domain");
            SystemError::ENXIO
        })?;

    irq_domain_manager().set_default_domain(intc_domain.clone());

    unsafe {
        RISCV_INTC_DOMAIN = Some(intc_domain.clone());
    }

    riscv_sbi_timer_irq_desc_init();

    return Ok(());
}

/// 把硬件中断号转换为riscv intc芯片的中断域的虚拟中断号
pub const fn riscv_intc_hwirq_to_virq(hwirq: HardwareIrqNumber) -> Option<IrqNumber> {
    if hwirq.data() < RiscvIntcChip::IRQ_SIZE {
        Some(IrqNumber::new(hwirq.data() + RISCV_INTC_VIRQ_START))
    } else {
        None
    }
}

/// 把riscv intc芯片的的中断域的虚拟中断号转换为硬件中断号
#[allow(dead_code)]
pub const fn riscv_intc_virq_to_hwirq(virq: IrqNumber) -> Option<HardwareIrqNumber> {
    if virq.data() >= RISCV_INTC_VIRQ_START
        && virq.data() < RISCV_INTC_VIRQ_START + RiscvIntcChip::IRQ_SIZE
    {
        Some(HardwareIrqNumber::new(virq.data() - RISCV_INTC_VIRQ_START))
    } else {
        None
    }
}

/// 将硬件中断号与riscv intc芯片的虚拟中断号关联
pub fn riscv_intc_assicate_irq(hwirq: HardwareIrqNumber) -> Option<IrqNumber> {
    let virq = riscv_intc_hwirq_to_virq(hwirq)?;
    irq_domain_manager()
        .domain_associate(
            riscv_intc_domain().as_ref().or_else(|| {
                error!("riscv_intc_domain is None");
                None
            })?,
            virq,
            hwirq,
        )
        .ok();

    Some(virq)
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/drivers/irqchip/irq-riscv-intc.c#23
pub fn riscv_intc_irq(trap_frame: &mut TrapFrame) {
    let hwirq = HardwareIrqNumber::new(trap_frame.cause.code() as u32);
    if hwirq.data() == 9 {
        // external interrupt
        do_plic_irq(trap_frame);
    } else {
        GenericIrqHandler::handle_domain_irq(
            riscv_intc_domain().clone().unwrap(),
            hwirq,
            trap_frame,
        )
        .ok();
    }
    do_softirq();
    if hwirq.data() == RiscVSbiTimer::TIMER_IRQ.data() {
        __schedule(SchedMode::SM_PREEMPT);
    }
}
