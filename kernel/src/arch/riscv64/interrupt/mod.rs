use riscv::register::{scause::Scause, sstatus::Sstatus};
use system_error::SystemError;

use crate::{
    driver::irqchip::riscv_intc::riscv_intc_init,
    exception::{InterruptArch, IrqFlags, IrqFlagsGuard, IrqNumber},
    libs::align::align_up,
};

use super::cpu::STACK_ALIGN;

pub(super) mod entry;
mod handle;
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
    pub epc: usize,
    pub ra: usize,
    pub sp: usize,
    pub gp: usize,
    pub tp: usize,
    pub t0: usize,
    pub t1: usize,
    pub t2: usize,
    pub s0: usize,
    pub s1: usize,
    pub a0: usize,
    pub a1: usize,
    pub a2: usize,
    pub a3: usize,
    pub a4: usize,
    pub a5: usize,
    pub a6: usize,
    pub a7: usize,
    pub s2: usize,
    pub s3: usize,
    pub s4: usize,
    pub s5: usize,
    pub s6: usize,
    pub s7: usize,
    pub s8: usize,
    pub s9: usize,
    pub s10: usize,
    pub s11: usize,
    pub t3: usize,
    pub t4: usize,
    pub t5: usize,
    pub t6: usize,
    // 以下是中断发生时自动保存的寄存器
    pub status: Sstatus,
    pub badaddr: usize,
    pub cause: Scause,
    /// a0 value before the syscall
    pub origin_a0: usize,
}

impl TrapFrame {
    /// 中断栈帧结构体的大小
    pub const SIZE: usize = core::mem::size_of::<TrapFrame>();

    /// 中断栈帧在栈上的大小
    pub const SIZE_ON_STACK: usize = align_up(Self::SIZE, STACK_ALIGN);
    /// 判断当前中断是否来自用户模式
    pub fn is_from_user(&self) -> bool {
        self.status.spp() == riscv::register::sstatus::SPP::User
    }
}
