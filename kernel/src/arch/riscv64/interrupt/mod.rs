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
    epc: usize,
    ra: usize,
    sp: usize,
    gp: usize,
    tp: usize,
    t0: usize,
    t1: usize,
    t2: usize,
    s0: usize,
    s1: usize,
    a0: usize,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
    a7: usize,
    s2: usize,
    s3: usize,
    s4: usize,
    s5: usize,
    s6: usize,
    s7: usize,
    s8: usize,
    s9: usize,
    s10: usize,
    s11: usize,
    t3: usize,
    t4: usize,
    t5: usize,
    t6: usize,
    // 以下是中断发生时自动保存的寄存器
    status: Sstatus,
    badaddr: usize,
    cause: Scause,
    /// a0 value before the syscall
    origin_a0: usize,
}

impl TrapFrame {
    /// 中断栈帧结构体的大小
    pub const SIZE: usize = core::mem::size_of::<TrapFrame>();

    /// 中断栈帧在栈上的大小
    pub const SIZE_ON_STACK: usize = align_up(Self::SIZE, STACK_ALIGN);
    /// 判断当前中断是否来自用户模式
    pub fn from_user(&self) -> bool {
        self.status.spp() == riscv::register::sstatus::SPP::User
    }
}
