use core::any::Any;
use kprobe::ProbeArgs;
use riscv::register::{scause::Scause, sstatus::Sstatus};
use system_error::SystemError;

use crate::{
    driver::irqchip::{riscv_intc::riscv_intc_init, riscv_sifive_plic::riscv_sifive_plic_init},
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
        Self::interrupt_disable();
        riscv_sifive_plic_init()?;
        // 注意，intc的初始化必须在plic之后，不然会导致plic无法关联上中断
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
        riscv::register::sstatus::clear_sie();
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

    pub fn new() -> Self {
        Self {
            epc: 0,
            ra: 0,
            sp: 0,
            gp: 0,
            tp: 0,
            t0: 0,
            t1: 0,
            t2: 0,
            s0: 0,
            s1: 0,
            a0: 0,
            a1: 0,
            a2: 0,
            a3: 0,
            a4: 0,
            a5: 0,
            a6: 0,
            a7: 0,
            s2: 0,
            s3: 0,
            s4: 0,
            s5: 0,
            s6: 0,
            s7: 0,
            s8: 0,
            s9: 0,
            s10: 0,
            s11: 0,
            t3: 0,
            t4: 0,
            t5: 0,
            t6: 0,
            status: unsafe { core::mem::zeroed() },
            badaddr: 0,
            cause: unsafe { core::mem::zeroed() },
            origin_a0: 0,
        }
    }

    pub fn set_return_value(&mut self, value: usize) {
        self.a0 = value;
    }

    /// 设置当前的程序计数器
    pub fn set_pc(&mut self, pc: usize) {
        self.epc = pc;
    }
}

impl ProbeArgs for TrapFrame {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn break_address(&self) -> usize {
        self.epc
    }
    fn debug_address(&self) -> usize {
        self.epc
    }
}
