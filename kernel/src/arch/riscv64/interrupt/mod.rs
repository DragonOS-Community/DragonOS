use crate::exception::{InterruptArch, IrqFlags, IrqFlagsGuard};

pub mod ipi;

pub struct RiscV64InterruptArch;

impl InterruptArch for RiscV64InterruptArch {
    unsafe fn interrupt_enable() {
        unimplemented!("RiscV64InterruptArch::interrupt_enable")
    }

    unsafe fn interrupt_disable() {
        unimplemented!("RiscV64InterruptArch::interrupt_disable")
    }

    fn is_irq_enabled() -> bool {
        unimplemented!("RiscV64InterruptArch::is_irq_enabled")
    }

    unsafe fn save_and_disable_irq() -> IrqFlagsGuard {
        unimplemented!("RiscV64InterruptArch::save_and_disable_irq")
    }

    unsafe fn restore_irq(flags: IrqFlags) {
        unimplemented!("RiscV64InterruptArch::restore_irq")
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
