use crate::exception::{InterruptArch, IrqFlags, IrqFlagsGuard};

pub mod ipi;

pub struct RiscV64InterruptArch;

impl InterruptArch for RiscV64InterruptArch {
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
