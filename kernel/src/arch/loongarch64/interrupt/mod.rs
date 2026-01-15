pub mod entry;
mod handle;
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

    /// 返回当前 TrapFrame 对应的用户态栈指针。
    #[inline(always)]
    pub fn stack_pointer(&self) -> usize {
        self.usp as usize
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

impl crate::process::rseq::RseqTrapFrame for TrapFrame {
    #[inline]
    fn rseq_ip(&self) -> usize {
        self.csr_era
    }

    #[inline]
    fn set_rseq_ip(&mut self, ip: usize) {
        self.csr_era = ip;
    }
}

/// Linux 兼容的用户寄存器结构体 (LoongArch64)
///
/// 严格按照 Linux 6.6.21 的 `arch/loongarch/include/uapi/asm/ptrace.h` 中的
///
/// 该结构体用于 ptrace 系统调用向用户空间暴露寄存器信息。
///
/// 参考: https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/include/uapi/asm/ptrace.h#30
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, Default)]
pub struct UserRegsStruct {
    /// 主处理器寄存器 (r0-r31)
    pub regs: [u64; 32],
    /// 原始系统调用参数 a0
    pub orig_a0: u64,
    /// CSR ERA (Exception Return Address)
    pub csr_era: u64,
    /// CSR BADV (Bad Virtual Address)
    pub csr_badv: u64,
    /// 保留字段
    pub reserved: [u64; 10],
}

impl UserRegsStruct {
    /// 从 TrapFrame 创建 UserRegsStruct
    ///
    /// 这对应 Linux 中从 pt_regs 构建 user_pt_regs 的过程。
    pub fn from_trap_frame(trap_frame: &TrapFrame) -> Self {
        let mut regs = [0u64; 32];
        // 按照 LoongArch 寄存器编号映射
        regs[0] = trap_frame.r0 as u64;
        regs[1] = trap_frame.ra as u64;
        regs[2] = trap_frame.tp as u64;
        regs[3] = trap_frame.usp as u64;
        regs[4] = trap_frame.a0 as u64;
        regs[5] = trap_frame.a1 as u64;
        regs[6] = trap_frame.a2 as u64;
        regs[7] = trap_frame.a3 as u64;
        regs[8] = trap_frame.a4 as u64;
        regs[9] = trap_frame.a5 as u64;
        regs[10] = trap_frame.a6 as u64;
        regs[11] = trap_frame.a7 as u64;
        regs[12] = trap_frame.t0 as u64;
        regs[13] = trap_frame.t1 as u64;
        regs[14] = trap_frame.t2 as u64;
        regs[15] = trap_frame.t3 as u64;
        regs[16] = trap_frame.t4 as u64;
        regs[17] = trap_frame.t5 as u64;
        regs[18] = trap_frame.t6 as u64;
        regs[19] = trap_frame.t7 as u64;
        regs[20] = trap_frame.t8 as u64;
        regs[21] = trap_frame.r21 as u64;
        regs[22] = trap_frame.fp as u64;
        regs[23] = trap_frame.s0 as u64;
        regs[24] = trap_frame.s1 as u64;
        regs[25] = trap_frame.s2 as u64;
        regs[26] = trap_frame.s3 as u64;
        regs[27] = trap_frame.s4 as u64;
        regs[28] = trap_frame.s5 as u64;
        regs[29] = trap_frame.s6 as u64;
        regs[30] = trap_frame.s7 as u64;
        regs[31] = trap_frame.s8 as u64;

        Self {
            regs,
            orig_a0: trap_frame.orig_a0 as u64,
            csr_era: trap_frame.csr_era as u64,
            csr_badv: trap_frame.csr_badvaddr as u64,
            reserved: [0; 10],
        }
    }

    /// 将 UserRegsStruct 的值写回 TrapFrame
    ///
    /// 用于 PTRACE_SETREGS 操作，允许调试器修改被跟踪进程的寄存器。
    pub fn write_to_trap_frame(&self, trap_frame: &mut TrapFrame) {
        trap_frame.r0 = self.regs[0] as usize;
        trap_frame.ra = self.regs[1] as usize;
        trap_frame.tp = self.regs[2] as usize;
        trap_frame.usp = self.regs[3] as usize;
        trap_frame.a0 = self.regs[4] as usize;
        trap_frame.a1 = self.regs[5] as usize;
        trap_frame.a2 = self.regs[6] as usize;
        trap_frame.a3 = self.regs[7] as usize;
        trap_frame.a4 = self.regs[8] as usize;
        trap_frame.a5 = self.regs[9] as usize;
        trap_frame.a6 = self.regs[10] as usize;
        trap_frame.a7 = self.regs[11] as usize;
        trap_frame.t0 = self.regs[12] as usize;
        trap_frame.t1 = self.regs[13] as usize;
        trap_frame.t2 = self.regs[14] as usize;
        trap_frame.t3 = self.regs[15] as usize;
        trap_frame.t4 = self.regs[16] as usize;
        trap_frame.t5 = self.regs[17] as usize;
        trap_frame.t6 = self.regs[18] as usize;
        trap_frame.t7 = self.regs[19] as usize;
        trap_frame.t8 = self.regs[20] as usize;
        trap_frame.r21 = self.regs[21] as usize;
        trap_frame.fp = self.regs[22] as usize;
        trap_frame.s0 = self.regs[23] as usize;
        trap_frame.s1 = self.regs[24] as usize;
        trap_frame.s2 = self.regs[25] as usize;
        trap_frame.s3 = self.regs[26] as usize;
        trap_frame.s4 = self.regs[27] as usize;
        trap_frame.s5 = self.regs[28] as usize;
        trap_frame.s6 = self.regs[29] as usize;
        trap_frame.s7 = self.regs[30] as usize;
        trap_frame.s8 = self.regs[31] as usize;
        trap_frame.orig_a0 = self.orig_a0 as usize;
        trap_frame.csr_era = self.csr_era as usize;
        trap_frame.csr_badvaddr = self.csr_badv as usize;
    }
}
