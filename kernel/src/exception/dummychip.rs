use alloc::sync::Arc;
use system_error::SystemError;

use super::{
    irqchip::{IrqChip, IrqChipFlags},
    irqdata::IrqData,
};

static mut NO_IRQ_CHIP: Option<Arc<NoIrqChip>> = None;
static mut DUMMY_IRQ_CHIP: Option<Arc<DummyIrqChip>> = None;

#[inline(never)]
pub fn no_irq_chip() -> Arc<dyn IrqChip> {
    unsafe { NO_IRQ_CHIP.as_ref().unwrap().clone() }
}

#[allow(dead_code)]
#[inline(never)]
pub fn dummy_irq_chip() -> Arc<dyn IrqChip> {
    unsafe { DUMMY_IRQ_CHIP.as_ref().unwrap().clone() }
}

fn ack_bad(_irq_data: &Arc<IrqData>) {
    todo!("ack_bad");
    // todo: https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/dummychip.c?fi=no_irq_chip#18
}

#[derive(Debug)]
struct NoIrqChip;

impl NoIrqChip {
    pub const fn new() -> Self {
        NoIrqChip
    }
}

impl IrqChip for NoIrqChip {
    fn name(&self) -> &'static str {
        "none"
    }
    fn irq_enable(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn irq_disable(&self, _irq: &Arc<IrqData>) {}

    fn irq_ack(&self, irq: &Arc<IrqData>) {
        ack_bad(irq);
    }

    fn irq_startup(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn irq_shutdown(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn flags(&self) -> IrqChipFlags {
        IrqChipFlags::IRQCHIP_SKIP_SET_WAKE
    }
}

#[derive(Debug)]
struct DummyIrqChip;

impl DummyIrqChip {
    pub const fn new() -> Self {
        DummyIrqChip
    }
}

impl IrqChip for DummyIrqChip {
    fn name(&self) -> &'static str {
        "dummy"
    }

    fn irq_enable(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn irq_disable(&self, _irq: &Arc<IrqData>) {}

    fn irq_ack(&self, _irq: &Arc<IrqData>) {}

    fn irq_mask(&self, _irq: &Arc<IrqData>) {}
    fn irq_unmask(&self, _irq: &Arc<IrqData>) {}

    fn irq_startup(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn irq_shutdown(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn flags(&self) -> IrqChipFlags {
        IrqChipFlags::IRQCHIP_SKIP_SET_WAKE
    }
}

#[inline(never)]
pub fn dummy_chip_init() {
    unsafe {
        NO_IRQ_CHIP = Some(Arc::new(NoIrqChip::new()));
        DUMMY_IRQ_CHIP = Some(Arc::new(DummyIrqChip::new()));
    }
}
