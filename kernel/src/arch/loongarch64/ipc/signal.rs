use crate::arch::interrupt::TrapFrame;
pub use crate::ipc::generic_signal::AtomicGenericSignal as AtomicSignal;
pub use crate::ipc::generic_signal::GenericSigChildCode as SigChildCode;
pub use crate::ipc::generic_signal::GenericSigSet as SigSet;
pub use crate::ipc::generic_signal::GenericSignal as Signal;
pub use crate::ipc::generic_signal::GENERIC_MAX_SIG_NUM as MAX_SIG_NUM;
pub use crate::ipc::generic_signal::GENERIC_STACK_ALIGN as STACK_ALIGN;

pub use crate::ipc::generic_signal::GenericSigFlags as SigFlags;

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
