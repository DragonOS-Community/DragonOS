pub use crate::ipc::generic_signal::AtomicGenericSignal as AtomicSignal;
pub use crate::ipc::generic_signal::GenericSigChildCode as SigChildCode;
pub use crate::ipc::generic_signal::GenericSigSet as SigSet;
pub use crate::ipc::generic_signal::GenericSignal as Signal;
use crate::{arch::interrupt::TrapFrame, ipc::signal_types::SignalArch};

pub use crate::ipc::generic_signal::GENERIC_MAX_SIG_NUM as MAX_SIG_NUM;

/// siginfo中的si_code的可选值
/// 请注意，当这个值小于0时，表示siginfo来自用户态，否则来自内核态
#[derive(Copy, Debug, Clone)]
#[repr(i32)]
pub enum SigCode {
    /// sent by kill, sigsend, raise
    User = 0,
    /// sent by kernel from somewhere
    Kernel = 0x80,
    /// 通过sigqueue发送
    Queue = -1,
    /// 定时器过期时发送
    Timer = -2,
    /// 当实时消息队列的状态发生改变时发送
    Mesgq = -3,
    /// 当异步IO完成时发送
    AsyncIO = -4,
    /// sent by queued SIGIO
    SigIO = -5,
}

impl SigCode {
    /// 为SigCode这个枚举类型实现从i32转换到枚举类型的转换函数
    #[allow(dead_code)]
    pub fn from_i32(x: i32) -> SigCode {
        match x {
            0 => Self::User,
            0x80 => Self::Kernel,
            -1 => Self::Queue,
            -2 => Self::Timer,
            -3 => Self::Mesgq,
            -4 => Self::AsyncIO,
            -5 => Self::SigIO,
            _ => panic!("signal code not valid"),
        }
    }
}

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
