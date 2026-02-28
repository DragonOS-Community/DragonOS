use crate::arch::interrupt::TrapFrame;

pub fn setup_single_step(frame: &mut TrapFrame, step_addr: usize) {
    frame.rflags |= 0x100;
    frame.set_pc(step_addr);
}

pub fn clear_single_step(frame: &mut TrapFrame, return_addr: usize) {
    frame.rflags &= !0x100;
    frame.set_pc(return_addr);
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct KProbeContext {
    pub r15: ::core::ffi::c_ulong,
    pub r14: ::core::ffi::c_ulong,
    pub r13: ::core::ffi::c_ulong,
    pub r12: ::core::ffi::c_ulong,
    pub rbp: ::core::ffi::c_ulong,
    pub rbx: ::core::ffi::c_ulong,
    pub r11: ::core::ffi::c_ulong,
    pub r10: ::core::ffi::c_ulong,
    pub r9: ::core::ffi::c_ulong,
    pub r8: ::core::ffi::c_ulong,
    pub rax: ::core::ffi::c_ulong,
    pub rcx: ::core::ffi::c_ulong,
    pub rdx: ::core::ffi::c_ulong,
    pub rsi: ::core::ffi::c_ulong,
    pub rdi: ::core::ffi::c_ulong,
    pub orig_rax: ::core::ffi::c_ulong,
    pub rip: ::core::ffi::c_ulong,
    pub cs: ::core::ffi::c_ulong,
    pub eflags: ::core::ffi::c_ulong,
    pub rsp: ::core::ffi::c_ulong,
    pub ss: ::core::ffi::c_ulong,
}

impl From<&TrapFrame> for KProbeContext {
    fn from(trap_frame: &TrapFrame) -> Self {
        Self {
            r15: trap_frame.r15,
            r14: trap_frame.r14,
            r13: trap_frame.r13,
            r12: trap_frame.r12,
            rbp: trap_frame.rbp,
            rbx: trap_frame.rbx,
            r11: trap_frame.r11,
            r10: trap_frame.r10,
            r9: trap_frame.r9,
            r8: trap_frame.r8,
            rax: trap_frame.rax,
            rcx: trap_frame.rcx,
            rdx: trap_frame.rdx,
            rsi: trap_frame.rsi,
            rdi: trap_frame.rdi,
            orig_rax: 0,
            rip: trap_frame.rip,
            cs: trap_frame.cs,
            eflags: trap_frame.rflags,
            rsp: trap_frame.rsp,
            ss: trap_frame.ss,
        }
    }
}

const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;

/// 获取当前架构标识
pub fn syscall_get_arch() -> u32 {
    AUDIT_ARCH_X86_64
}

/// 从 KProbeContext 获取指令指针 (rip)
pub fn instruction_pointer(ctx: &KProbeContext) -> u64 {
    ctx.rip
}

/// 从 KProbeContext 获取用户栈指针 (rsp)
pub fn user_stack_pointer(ctx: &KProbeContext) -> u64 {
    ctx.rsp
}

/// 从 KProbeContext 获取系统调用号 (rax)
pub fn syscall_get_nr(ctx: &KProbeContext) -> u64 {
    ctx.rax
}

/// 从 KProbeContext 获取系统调用返回值 (rax)
pub fn syscall_get_return_value(ctx: &KProbeContext) -> i64 {
    ctx.rax as i64
}

/// 从 KProbeContext 获取系统调用的前 6 个参数
/// (遵循 x86_64 System V ABI)
pub fn syscall_get_arguments(ctx: &KProbeContext, args: &mut [u64; 6]) {
    args[0] = ctx.rdi;
    args[1] = ctx.rsi;
    args[2] = ctx.rdx;
    args[3] = ctx.r10;
    args[4] = ctx.r8;
    args[5] = ctx.r9;
}
