use crate::arch::interrupt::TrapFrame;

pub fn setup_single_step(frame: &mut TrapFrame, step_addr: usize) {
    // LoongArch64 单步调试需要设置 CSR.FWPS 寄存器
    // 目前先设置 PC 到目标地址
    frame.csr_era = step_addr;
}

pub fn clear_single_step(frame: &mut TrapFrame, return_addr: usize) {
    // 清除单步调试状态并设置返回地址
    frame.csr_era = return_addr;
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct KProbeContext {
    pub r0: usize,
    pub ra: usize,
    pub tp: usize,
    pub sp: usize,
    pub a0: usize,
    pub a1: usize,
    pub a2: usize,
    pub a3: usize,
    pub a4: usize,
    pub a5: usize,
    pub a6: usize,
    pub a7: usize,
    pub t0: usize,
    pub t1: usize,
    pub t2: usize,
    pub t3: usize,
    pub t4: usize,
    pub t5: usize,
    pub t6: usize,
    pub t7: usize,
    pub t8: usize,
    pub r21: usize,
    pub fp: usize,
    pub s0: usize,
    pub s1: usize,
    pub s2: usize,
    pub s3: usize,
    pub s4: usize,
    pub s5: usize,
    pub s6: usize,
    pub s7: usize,
    pub s8: usize,
    pub orig_a0: usize,
    pub csr_era: usize,
    pub csr_badvaddr: usize,
    pub csr_crmd: usize,
    pub csr_prmd: usize,
    pub csr_euen: usize,
    pub csr_ecfg: usize,
    pub csr_estat: usize,
}

impl From<&TrapFrame> for KProbeContext {
    fn from(trap_frame: &TrapFrame) -> Self {
        Self {
            r0: trap_frame.r0,
            ra: trap_frame.ra,
            tp: trap_frame.tp,
            sp: trap_frame.usp,
            a0: trap_frame.a0,
            a1: trap_frame.a1,
            a2: trap_frame.a2,
            a3: trap_frame.a3,
            a4: trap_frame.a4,
            a5: trap_frame.a5,
            a6: trap_frame.a6,
            a7: trap_frame.a7,
            t0: trap_frame.t0,
            t1: trap_frame.t1,
            t2: trap_frame.t2,
            t3: trap_frame.t3,
            t4: trap_frame.t4,
            t5: trap_frame.t5,
            t6: trap_frame.t6,
            t7: trap_frame.t7,
            t8: trap_frame.t8,
            r21: trap_frame.r21,
            fp: trap_frame.fp,
            s0: trap_frame.s0,
            s1: trap_frame.s1,
            s2: trap_frame.s2,
            s3: trap_frame.s3,
            s4: trap_frame.s4,
            s5: trap_frame.s5,
            s6: trap_frame.s6,
            s7: trap_frame.s7,
            s8: trap_frame.s8,
            orig_a0: trap_frame.orig_a0,
            csr_era: trap_frame.csr_era,
            csr_badvaddr: trap_frame.csr_badvaddr,
            csr_crmd: trap_frame.csr_crmd,
            csr_prmd: trap_frame.csr_prmd,
            csr_euen: trap_frame.csr_euen,
            csr_ecfg: trap_frame.csr_ecfg,
            csr_estat: trap_frame.csr_estat,
        }
    }
}

// LoongArch 64-bit 架构标识 (EM_LOONGARCH = 258, 64-bit)
const AUDIT_ARCH_LOONGARCH64: u32 = 0xC000_0102;

/// 获取当前架构标识
pub fn syscall_get_arch() -> u32 {
    AUDIT_ARCH_LOONGARCH64
}

/// 从 KProbeContext 获取指令指针 (csr_era)
pub fn instruction_pointer(ctx: &KProbeContext) -> u64 {
    ctx.csr_era as u64
}

/// 从 KProbeContext 获取用户栈指针 (sp)
pub fn user_stack_pointer(ctx: &KProbeContext) -> u64 {
    ctx.sp as u64
}

/// 从 KProbeContext 获取系统调用号 (a7)
/// LoongArch64 使用 a7 寄存器传递系统调用号
pub fn syscall_get_nr(ctx: &KProbeContext) -> u64 {
    ctx.a7 as u64
}

/// 从 KProbeContext 获取系统调用返回值 (a0)
pub fn syscall_get_return_value(ctx: &KProbeContext) -> i64 {
    ctx.a0 as i64
}

/// 从 KProbeContext 获取系统调用的前 6 个参数
/// LoongArch64 使用 a0-a5 寄存器传递系统调用参数
pub fn syscall_get_arguments(ctx: &KProbeContext, args: &mut [u64; 6]) {
    args[0] = ctx.a0 as u64;
    args[1] = ctx.a1 as u64;
    args[2] = ctx.a2 as u64;
    args[3] = ctx.a3 as u64;
    args[4] = ctx.a4 as u64;
    args[5] = ctx.a5 as u64;
}
