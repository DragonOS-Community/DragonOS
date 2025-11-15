use crate::arch::interrupt::TrapFrame;
pub use crate::ipc::generic_signal::AtomicGenericSignal as AtomicSignal;
pub use crate::ipc::generic_signal::GenericSigChildCode as SigChildCode;
pub use crate::ipc::generic_signal::GenericSigFlags as SigFlags;
pub use crate::ipc::generic_signal::GenericSigSet as SigSet;
pub use crate::ipc::generic_signal::GenericSigStackFlags as SigStackFlags;
pub use crate::ipc::generic_signal::GenericSignal as Signal;

pub use crate::ipc::generic_signal::GENERIC_MAX_SIG_NUM as MAX_SIG_NUM;
pub use crate::ipc::generic_signal::GENERIC_STACK_ALIGN as STACK_ALIGN;

use crate::ipc::signal_types::SignalArch;

pub struct LoongArch64SignalArch;

impl SignalArch for LoongArch64SignalArch {
    // TODO: 为LoongArch64实现信号处理
    // 注意，la64现在在中断/系统调用返回用户态时，没有进入 irqentry_exit() 函数，
    // 到时候实现信号处理时，需要修改中断/系统调用返回用户态的代码，进入 irqentry_exit() 函数
    unsafe fn do_signal_or_restart(_frame: &mut TrapFrame) {
        todo!("la64:do_signal_or_restart")
    }

    fn sys_rt_sigreturn(_trap_frame: &mut TrapFrame) -> u64 {
        todo!("la64:sys_rt_sigreturn")
    }
}

/// @brief 信号处理备用栈的信息
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct LoongArch64SigStack {
    pub sp: usize,
    pub flags: SigStackFlags,
    pub size: u32,
}

impl LoongArch64SigStack {
    pub fn new() -> Self {
        Self {
            sp: 0,
            flags: SigStackFlags::SS_DISABLE,
            size: 0,
        }
    }

    /// 检查给定的栈指针 `sp` 是否在当前备用信号栈的范围内。
    #[inline]
    pub fn on_sig_stack(&self, sp: usize) -> bool {
        self.sp != 0 && self.size != 0 && (sp.wrapping_sub(self.sp) < self.size as usize)
    }
}

impl Default for LoongArch64SigStack {
    fn default() -> Self {
        Self::new()
    }
}
