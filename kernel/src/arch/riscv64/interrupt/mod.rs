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

    /// 返回当前 TrapFrame 对应的用户态栈指针。
    #[inline(always)]
    pub fn stack_pointer(&self) -> usize {
        self.sp
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

impl crate::process::rseq::RseqTrapFrame for TrapFrame {
    #[inline]
    fn rseq_ip(&self) -> usize {
        self.epc
    }

    #[inline]
    fn set_rseq_ip(&mut self, ip: usize) {
        self.epc = ip;
    }
}

/// Linux 兼容的用户寄存器结构体 (RISC-V 64)
///
/// 该结构体用于 ptrace 系统调用向用户空间暴露寄存器信息。
///
/// 参考: https://code.dragonos.org.cn/xref/linux-6.6.21/arch/riscv/include/uapi/asm/ptrace.h#24
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct UserRegsStruct {
    pub pc: u64,
    pub ra: u64,
    pub sp: u64,
    pub gp: u64,
    pub tp: u64,
    pub t0: u64,
    pub t1: u64,
    pub t2: u64,
    pub s0: u64,
    pub s1: u64,
    pub a0: u64,
    pub a1: u64,
    pub a2: u64,
    pub a3: u64,
    pub a4: u64,
    pub a5: u64,
    pub a6: u64,
    pub a7: u64,
    pub s2: u64,
    pub s3: u64,
    pub s4: u64,
    pub s5: u64,
    pub s6: u64,
    pub s7: u64,
    pub s8: u64,
    pub s9: u64,
    pub s10: u64,
    pub s11: u64,
    pub t3: u64,
    pub t4: u64,
    pub t5: u64,
    pub t6: u64,
}

impl UserRegsStruct {
    /// 从 TrapFrame 创建 UserRegsStruct
    ///
    /// 这对应 Linux 中从 pt_regs 构建 user_regs_struct 的过程。
    /// RISC-V 的 user_regs_struct 是 pt_regs 的前缀。
    pub fn from_trap_frame(trap_frame: &TrapFrame) -> Self {
        Self {
            pc: trap_frame.epc as u64,
            ra: trap_frame.ra as u64,
            sp: trap_frame.sp as u64,
            gp: trap_frame.gp as u64,
            tp: trap_frame.tp as u64,
            t0: trap_frame.t0 as u64,
            t1: trap_frame.t1 as u64,
            t2: trap_frame.t2 as u64,
            s0: trap_frame.s0 as u64,
            s1: trap_frame.s1 as u64,
            a0: trap_frame.a0 as u64,
            a1: trap_frame.a1 as u64,
            a2: trap_frame.a2 as u64,
            a3: trap_frame.a3 as u64,
            a4: trap_frame.a4 as u64,
            a5: trap_frame.a5 as u64,
            a6: trap_frame.a6 as u64,
            a7: trap_frame.a7 as u64,
            s2: trap_frame.s2 as u64,
            s3: trap_frame.s3 as u64,
            s4: trap_frame.s4 as u64,
            s5: trap_frame.s5 as u64,
            s6: trap_frame.s6 as u64,
            s7: trap_frame.s7 as u64,
            s8: trap_frame.s8 as u64,
            s9: trap_frame.s9 as u64,
            s10: trap_frame.s10 as u64,
            s11: trap_frame.s11 as u64,
            t3: trap_frame.t3 as u64,
            t4: trap_frame.t4 as u64,
            t5: trap_frame.t5 as u64,
            t6: trap_frame.t6 as u64,
        }
    }

    /// 将 UserRegsStruct 的值写回 TrapFrame
    ///
    /// 用于 PTRACE_SETREGS 操作，允许调试器修改被跟踪进程的寄存器。
    pub fn write_to_trap_frame(&self, trap_frame: &mut TrapFrame) {
        trap_frame.epc = self.pc as usize;
        trap_frame.ra = self.ra as usize;
        trap_frame.sp = self.sp as usize;
        trap_frame.gp = self.gp as usize;
        trap_frame.tp = self.tp as usize;
        trap_frame.t0 = self.t0 as usize;
        trap_frame.t1 = self.t1 as usize;
        trap_frame.t2 = self.t2 as usize;
        trap_frame.s0 = self.s0 as usize;
        trap_frame.s1 = self.s1 as usize;
        trap_frame.a0 = self.a0 as usize;
        trap_frame.a1 = self.a1 as usize;
        trap_frame.a2 = self.a2 as usize;
        trap_frame.a3 = self.a3 as usize;
        trap_frame.a4 = self.a4 as usize;
        trap_frame.a5 = self.a5 as usize;
        trap_frame.a6 = self.a6 as usize;
        trap_frame.a7 = self.a7 as usize;
        trap_frame.s2 = self.s2 as usize;
        trap_frame.s3 = self.s3 as usize;
        trap_frame.s4 = self.s4 as usize;
        trap_frame.s5 = self.s5 as usize;
        trap_frame.s6 = self.s6 as usize;
        trap_frame.s7 = self.s7 as usize;
        trap_frame.s8 = self.s8 as usize;
        trap_frame.s9 = self.s9 as usize;
        trap_frame.s10 = self.s10 as usize;
        trap_frame.s11 = self.s11 as usize;
        trap_frame.t3 = self.t3 as usize;
        trap_frame.t4 = self.t4 as usize;
        trap_frame.t5 = self.t5 as usize;
        trap_frame.t6 = self.t6 as usize;
    }
}
