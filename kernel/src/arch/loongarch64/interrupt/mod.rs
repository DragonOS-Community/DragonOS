pub mod ipi;

use core::any::Any;

use kprobe::ProbeArgs;
use loongArch64::register::CpuMode;

use crate::exception::{InterruptArch, IrqFlags, IrqFlagsGuard, IrqNumber};

pub struct LoongArch64InterruptArch;

impl InterruptArch for LoongArch64InterruptArch {
    unsafe fn arch_irq_init() -> Result<(), system_error::SystemError> {
        todo!("arch_irq_init() not implemented for LoongArch64InterruptArch")
    }

    unsafe fn interrupt_enable() {
        loongArch64::register::crmd::set_ie(true);
    }

    unsafe fn interrupt_disable() {
        loongArch64::register::crmd::set_ie(false);
    }

    fn is_irq_enabled() -> bool {
        loongArch64::register::crmd::read().ie()
    }

    unsafe fn save_and_disable_irq() -> IrqFlagsGuard {
        let ie = loongArch64::register::crmd::read().ie();
        loongArch64::register::crmd::set_ie(false);
        IrqFlagsGuard::new(IrqFlags::new(if ie { 1 } else { 0 }))
    }

    unsafe fn restore_irq(flags: IrqFlags) {
        loongArch64::register::crmd::set_ie(flags.flags() == 1);
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
pub struct TrapFrame {
    pub r0: usize,  // 0*8
    pub ra: usize,  // 1*8
    pub tp: usize,  // 2*8
    pub usp: usize, // 3*8 (user stack pointer)
    pub a0: usize,  // 4*8
    pub a1: usize,  // 5*8
    pub a2: usize,  // 6*8
    pub a3: usize,  // 7*8
    pub a4: usize,  // 8*8
    pub a5: usize,  // 9*8
    pub a6: usize,  // 10*8
    pub a7: usize,  // 11*8
    pub t0: usize,  // 12*8
    pub t1: usize,  // 13*8
    pub t2: usize,  // 14*8
    pub t3: usize,  // 15*8
    pub t4: usize,  // 16*8
    pub t5: usize,  // 17*8
    pub t6: usize,  // 18*8
    pub t7: usize,  // 19*8
    pub t8: usize,  // 20*8
    pub r21: usize, // 21*8
    pub fp: usize,  // 22*8
    pub s0: usize,  // 23*8
    pub s1: usize,  // 24*8
    pub s2: usize,  // 25*8
    pub s3: usize,  // 26*8
    pub s4: usize,  // 27*8
    pub s5: usize,  // 28*8
    pub s6: usize,  // 29*8
    pub s7: usize,  // 30*8
    pub s8: usize,  // 31*8
    /// original syscall arg0
    pub orig_a0: usize,

    pub csr_era: usize,
    pub csr_badvaddr: usize,
    pub csr_crmd: usize,
    pub csr_prmd: usize,
    pub csr_euen: usize,
    pub csr_ecfg: usize,
    pub csr_estat: usize,
}

impl TrapFrame {
    /// 中断栈帧结构体的大小
    pub const SIZE: usize = core::mem::size_of::<TrapFrame>();

    /// 判断当前中断是否来自用户模式
    pub fn is_from_user(&self) -> bool {
        loongArch64::register::crmd::Crmd::from(self.csr_crmd).plv() == CpuMode::Ring3
    }

    #[inline(never)]
    pub const fn new() -> Self {
        let x = core::mem::MaybeUninit::<Self>::zeroed();
        unsafe { x.assume_init() }
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
