pub mod ipi;

use core::any::Any;

use kprobe::ProbeArgs;

use crate::{
    exception::{InterruptArch, IrqFlags, IrqFlagsGuard, IrqNumber},
    libs::align::align_up,
};

pub struct LoongArch64InterruptArch;

impl InterruptArch for LoongArch64InterruptArch {
    unsafe fn arch_irq_init() -> Result<(), system_error::SystemError> {
        todo!("arch_irq_init() not implemented for LoongArch64InterruptArch")
    }

    unsafe fn interrupt_enable() {
        todo!("interrupt_enable() not implemented for LoongArch64InterruptArch")
    }

    unsafe fn interrupt_disable() {
        todo!("interrupt_disable() not implemented for LoongArch64InterruptArch")
    }

    fn is_irq_enabled() -> bool {
        todo!("is_irq_enabled() not implemented for LoongArch64InterruptArch")
    }

    unsafe fn save_and_disable_irq() -> IrqFlagsGuard {
        todo!("save_and_disable_irq() not implemented for LoongArch64InterruptArch")
    }

    unsafe fn restore_irq(flags: IrqFlags) {
        todo!("restore_irq() not implemented for LoongArch64InterruptArch")
    }

    fn probe_total_irq_num() -> u32 {
        todo!("probe_total_irq_num() not implemented for LoongArch64InterruptArch")
    }

    fn ack_bad_irq(irq: IrqNumber) {
        todo!("ack_bad_irq() not implemented for LoongArch64InterruptArch")
    }
}

/// 中断栈帧结构体
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct TrapFrame {}

impl TrapFrame {
    /// 中断栈帧结构体的大小
    pub const SIZE: usize = core::mem::size_of::<TrapFrame>();

    /// 判断当前中断是否来自用户模式
    pub fn is_from_user(&self) -> bool {
        todo!("TrapFrame::is_from_user()")
    }

    pub fn new() -> Self {
        TrapFrame {}
    }
}

impl ProbeArgs for TrapFrame {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn break_address(&self) -> usize {
        todo!("TrapFrame::break_address()")
    }

    fn debug_address(&self) -> usize {
        todo!("TrapFrame::debug_address()")
    }
}
