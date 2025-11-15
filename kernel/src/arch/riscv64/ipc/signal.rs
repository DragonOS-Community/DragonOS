pub use crate::ipc::generic_signal::AtomicGenericSignal as AtomicSignal;
pub use crate::ipc::generic_signal::GenericSigChildCode as SigChildCode;
pub use crate::ipc::generic_signal::GenericSigSet as SigSet;
pub use crate::ipc::generic_signal::GenericSigStackFlags as SigStackFlags;
pub use crate::ipc::generic_signal::GenericSignal as Signal;

pub use crate::ipc::generic_signal::GENERIC_MAX_SIG_NUM as MAX_SIG_NUM;

use crate::{
    arch::interrupt::TrapFrame,
    ipc::signal_types::{SigCode, SignalArch},
};

pub struct RiscV64SignalArch;

impl SignalArch for RiscV64SignalArch {
    // TODO: 为RISCV64实现信号处理
    // 注意，rv64现在在中断/系统调用返回用户态时，没有进入 irqentry_exit() 函数，
    // 到时候实现信号处理时，需要修改中断/系统调用返回用户态的代码，进入 irqentry_exit() 函数
    unsafe fn do_signal_or_restart(_frame: &mut TrapFrame) {
        todo!()
    }

    fn sys_rt_sigreturn(_trap_frame: &mut TrapFrame) -> u64 {
        todo!()
    }
}

bitflags! {
    #[repr(C,align(8))]
    #[derive(Default)]
    pub struct SigFlags:u32{
        const SA_NOCLDSTOP =  1;
        const SA_NOCLDWAIT = 2;
        const SA_SIGINFO   = 4;
        const SA_ONSTACK   = 0x08000000;
        const SA_RESTART   = 0x10000000;
        const SA_NODEFER  = 0x40000000;
        const SA_RESETHAND = 0x80000000;
        const SA_RESTORER   =0x04000000;
        const SA_ALL = Self::SA_NOCLDSTOP.bits()|Self::SA_NOCLDWAIT.bits()|Self::SA_NODEFER.bits()|Self::SA_ONSTACK.bits()|Self::SA_RESETHAND.bits()|Self::SA_RESTART.bits()|Self::SA_SIGINFO.bits()|Self::SA_RESTORER.bits();
    }

}

/// @brief 信号处理备用栈的信息
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct RiscV64SigStack {
    pub sp: usize,
    pub flags: u32,
    pub size: u32,
}

impl RiscV64SigStack {
    pub fn new() -> Self {
        Self {
            sp: 0,
            flags: 0,
            size: 0,
        }
    }

    /// 检查给定的栈指针 `sp` 是否在当前备用信号栈的范围内。
    #[inline]
    pub fn on_sig_stack(&self, sp: usize) -> bool {
        self.sp != 0 && self.size != 0 && (sp.wrapping_sub(self.sp) < self.size as usize)
    }
}

impl Default for RiscV64SigStack {
    fn default() -> Self {
        Self::new()
    }
}
