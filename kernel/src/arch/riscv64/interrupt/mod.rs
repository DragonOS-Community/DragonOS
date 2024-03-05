use system_error::SystemError;

use crate::{
    driver::irqchip::riscv_intc::riscv_intc_init,
    exception::{InterruptArch, IrqFlags, IrqFlagsGuard, IrqNumber},
};

pub mod ipi;

pub struct RiscV64InterruptArch;

impl InterruptArch for RiscV64InterruptArch {
    unsafe fn arch_irq_init() -> Result<(), SystemError> {
        riscv_intc_init()?;

        Ok(())
    }
    unsafe fn interrupt_enable() {
        riscv::interrupt::enable();
    }

    unsafe fn interrupt_disable() {
        riscv::interrupt::disable();
    }

    fn is_irq_enabled() -> bool {
        riscv::register::sstatus::read().sie()
    }

    unsafe fn save_and_disable_irq() -> IrqFlagsGuard {
        let sie = riscv::register::sstatus::read().sie();
        IrqFlagsGuard::new(IrqFlags::new(sie.into()))
    }

    unsafe fn restore_irq(flags: IrqFlags) {
        let sie: bool = flags.flags() != 0;
        if sie {
            riscv::register::sstatus::set_sie();
        } else {
            riscv::register::sstatus::clear_sie();
        }
    }

    fn probe_total_irq_num() -> u32 {
        // todo: 获取中断总数
        256
    }

    fn ack_bad_irq(irq: IrqNumber) {
        todo!("ack_bad_irq: {}", irq.data());
    }
}

/// 中断栈帧结构体
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct TrapFrame {
    // todo
}

impl TrapFrame {
    /// 判断当前中断是否来自用户模式
    pub fn from_user(&self) -> bool {
        unimplemented!("TrapFrame::from_user")
    }
}
