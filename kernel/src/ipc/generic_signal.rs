use num_traits::FromPrimitive;

use crate::arch::ipc::signal::{SigSet, MAX_SIG_NUM};

/// 信号处理的栈的栈指针的最小对齐
#[allow(dead_code)]
pub const GENERIC_STACK_ALIGN: u64 = 16;
/// 信号最大值
#[allow(dead_code)]
pub const GENERIC_MAX_SIG_NUM: usize = 64;

#[allow(dead_code)]
#[derive(Eq, PartialEq, FromPrimitive)]
#[repr(usize)]
#[allow(non_camel_case_types)]
#[atomic_enum]
pub enum GenericSignal {
    INVALID = 0,
    SIGHUP = 1,
    SIGINT,
    SIGQUIT,
    SIGILL,
    SIGTRAP,
    /// SIGABRT和SIGIOT共用这个号码
    SIGABRT_OR_IOT,
    SIGBUS,
    SIGFPE,
    SIGKILL,
    SIGUSR1,

    SIGSEGV = 11,
    SIGUSR2,
    SIGPIPE,
    SIGALRM,
    SIGTERM,
    SIGSTKFLT,
    SIGCHLD,
    SIGCONT,
    SIGSTOP,
    SIGTSTP,

    SIGTTIN = 21,
    SIGTTOU,
    SIGURG,
    SIGXCPU,
    SIGXFSZ,
    SIGVTALRM,
    SIGPROF,
    SIGWINCH,
    /// SIGIO和SIGPOLL共用这个号码
    SIGIO_OR_POLL,
    SIGPWR,

    SIGSYS = 31,

    SIGRTMIN = 32,
    SIGRTMAX = 64,
}

impl GenericSignal {
    /// 判断一个数字是否为可用的信号
    #[inline]
    pub fn is_valid(&self) -> bool {
        return (*self) as usize <= MAX_SIG_NUM;
    }

    /// const convertor between `Signal` and `SigSet`
    pub const fn into_sigset(self) -> SigSet {
        SigSet::from_bits_truncate(1 << (self as usize - 1))
    }

    /// 判断一个信号是不是实时信号
    ///
    /// ## 返回值
    ///
    /// - `true` 这个信号是实时信号
    /// - `false` 这个信号不是实时信号
    #[inline]
    pub fn is_rt_signal(&self) -> bool {
        return (*self) as usize >= Self::SIGRTMIN.into();
    }
}

impl From<GenericSignal> for usize {
    fn from(val: GenericSignal) -> Self {
        val as usize
    }
}

impl From<usize> for GenericSignal {
    fn from(value: usize) -> Self {
        <Self as FromPrimitive>::from_usize(value).unwrap_or(GenericSignal::INVALID)
    }
}

impl From<i32> for GenericSignal {
    fn from(value: i32) -> Self {
        if value < 0 {
            log::error!("Try to convert an invalid number to GenericSignal");
            return GenericSignal::INVALID;
        } else {
            return Self::from(value as usize);
        }
    }
}

impl From<GenericSignal> for GenericSigSet {
    fn from(val: GenericSignal) -> Self {
        GenericSigSet {
            bits: (1 << (val as usize - 1) as u64),
        }
    }
}

/// SIGCHLD si_codes
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToPrimitive)]
#[allow(dead_code)]
pub enum GenericSigChildCode {
    /// child has exited
    ///
    /// CLD_EXITED
    Exited = 1,
    /// child was killed
    ///
    /// CLD_KILLED
    Killed = 2,
    /// child terminated abnormally
    ///
    /// CLD_DUMPED
    Dumped = 3,
    /// traced child has trapped
    ///
    /// CLD_TRAPPED
    Trapped = 4,
    /// child has stopped
    ///
    /// CLD_STOPPED
    Stopped = 5,
    /// stopped child has continued
    ///
    /// CLD_CONTINUED
    Continued = 6,
}

impl From<GenericSigChildCode> for i32 {
    fn from(value: GenericSigChildCode) -> Self {
        value as i32
    }
}

bitflags! {
    /// 请注意，sigset 这个bitmap, 第0位表示sig=1的信号。也就是说，Signal-1才是sigset_t中对应的位
    #[derive(Default)]
    pub struct GenericSigSet:u64 {
        const SIGHUP   =  1<<0;
        const SIGINT   =  1<<1;
        const SIGQUIT  =  1<<2;
        const SIGILL   =  1<<3;
        const SIGTRAP  =  1<<4;
        /// SIGABRT和SIGIOT共用这个号码
        const SIGABRT_OR_IOT    =    1<<5;
        const SIGBUS   =  1<<6;
        const SIGFPE   =  1<<7;
        const SIGKILL  =  1<<8;
        const SIGUSR   =  1<<9;
        const SIGSEGV  =  1<<10;
        const SIGUSR2  =  1<<11;
        const SIGPIPE  =  1<<12;
        const SIGALRM  =  1<<13;
        const SIGTERM  =  1<<14;
        const SIGSTKFLT=  1<<15;
        const SIGCHLD  =  1<<16;
        const SIGCONT  =  1<<17;
        const SIGSTOP  =  1<<18;
        const SIGTSTP  =  1<<19;
        const SIGTTIN  =  1<<20;
        const SIGTTOU  =  1<<21;
        const SIGURG   =  1<<22;
        const SIGXCPU  =  1<<23;
        const SIGXFSZ  =  1<<24;
        const SIGVTALRM=  1<<25;
        const SIGPROF  =  1<<26;
        const SIGWINCH =  1<<27;
        /// SIGIO和SIGPOLL共用这个号码
        const SIGIO_OR_POLL    =   1<<28;
        const SIGPWR   =  1<<29;
        const SIGSYS   =  1<<30;
        const SIGRTMIN =  1<<31;
        // TODO 写上实时信号
        const SIGRTMAX =  1 << (GENERIC_MAX_SIG_NUM-1);
    }

    #[repr(C,align(8))]
    #[derive(Default)]
    pub struct GenericSigFlags:u32{
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
