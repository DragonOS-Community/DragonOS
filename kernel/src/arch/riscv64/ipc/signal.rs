use crate::{
    arch::{sched::sched, CurrentIrqArch},
    exception::InterruptArch,
    kerror,
    process::ProcessManager,
};

/// 信号最大值
pub const MAX_SIG_NUM: usize = 64;
#[allow(dead_code)]
#[derive(Eq)]
#[repr(usize)]
#[allow(non_camel_case_types)]
#[atomic_enum]
pub enum Signal {
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

/// 为Signal实现判断相等的trait
impl PartialEq for Signal {
    fn eq(&self, other: &Signal) -> bool {
        *self as usize == *other as usize
    }
}

impl From<usize> for Signal {
    fn from(value: usize) -> Self {
        if value <= MAX_SIG_NUM {
            let ret: Signal = unsafe { core::mem::transmute(value) };
            return ret;
        } else {
            kerror!("Try to convert an invalid number to Signal");
            return Signal::INVALID;
        }
    }
}

impl Into<usize> for Signal {
    fn into(self) -> usize {
        self as usize
    }
}

impl From<i32> for Signal {
    fn from(value: i32) -> Self {
        if value < 0 {
            kerror!("Try to convert an invalid number to Signal");
            return Signal::INVALID;
        } else {
            return Self::from(value as usize);
        }
    }
}

impl Into<SigSet> for Signal {
    fn into(self) -> SigSet {
        SigSet {
            bits: (1 << (self as usize - 1) as u64),
        }
    }
}
impl Signal {
    /// 判断一个数字是否为可用的信号
    #[inline]
    pub fn is_valid(&self) -> bool {
        return (*self) as usize <= MAX_SIG_NUM;
    }

    /// const convertor between `Signal` and `SigSet`
    pub const fn into_sigset(self) -> SigSet {
        SigSet {
            bits: (1 << (self as usize - 1) as u64),
        }
    }

    /// 判断一个信号是不是实时信号
    ///
    /// ## 返回值
    ///
    /// - `true` 这个信号是实时信号
    /// - `false` 这个信号不是实时信号
    #[inline]
    pub fn is_rt_signal(&self) -> bool {
        return (*self) as usize >= Signal::SIGRTMIN.into();
    }

    /// 调用信号的默认处理函数
    pub fn handle_default(&self) {
        match self {
            Signal::INVALID => {
                kerror!("attempting to handler an Invalid");
            }
            Signal::SIGHUP => sig_terminate(self.clone()),
            Signal::SIGINT => sig_terminate(self.clone()),
            Signal::SIGQUIT => sig_terminate_dump(self.clone()),
            Signal::SIGILL => sig_terminate_dump(self.clone()),
            Signal::SIGTRAP => sig_terminate_dump(self.clone()),
            Signal::SIGABRT_OR_IOT => sig_terminate_dump(self.clone()),
            Signal::SIGBUS => sig_terminate_dump(self.clone()),
            Signal::SIGFPE => sig_terminate_dump(self.clone()),
            Signal::SIGKILL => sig_terminate(self.clone()),
            Signal::SIGUSR1 => sig_terminate(self.clone()),
            Signal::SIGSEGV => sig_terminate_dump(self.clone()),
            Signal::SIGUSR2 => sig_terminate(self.clone()),
            Signal::SIGPIPE => sig_terminate(self.clone()),
            Signal::SIGALRM => sig_terminate(self.clone()),
            Signal::SIGTERM => sig_terminate(self.clone()),
            Signal::SIGSTKFLT => sig_terminate(self.clone()),
            Signal::SIGCHLD => sig_ignore(self.clone()),
            Signal::SIGCONT => sig_continue(self.clone()),
            Signal::SIGSTOP => sig_stop(self.clone()),
            Signal::SIGTSTP => sig_stop(self.clone()),
            Signal::SIGTTIN => sig_stop(self.clone()),
            Signal::SIGTTOU => sig_stop(self.clone()),
            Signal::SIGURG => sig_ignore(self.clone()),
            Signal::SIGXCPU => sig_terminate_dump(self.clone()),
            Signal::SIGXFSZ => sig_terminate_dump(self.clone()),
            Signal::SIGVTALRM => sig_terminate(self.clone()),
            Signal::SIGPROF => sig_terminate(self.clone()),
            Signal::SIGWINCH => sig_ignore(self.clone()),
            Signal::SIGIO_OR_POLL => sig_terminate(self.clone()),
            Signal::SIGPWR => sig_terminate(self.clone()),
            Signal::SIGSYS => sig_terminate(self.clone()),
            Signal::SIGRTMIN => sig_terminate(self.clone()),
            Signal::SIGRTMAX => sig_terminate(self.clone()),
        }
    }
}

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

    /// 请注意，sigset 这个bitmap, 第0位表示sig=1的信号。也就是说，Signal-1才是sigset_t中对应的位
    #[derive(Default)]
    pub struct SigSet:u64{
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
        const SIGRTMAX =  1<<MAX_SIG_NUM-1;
    }
}

/// SIGCHLD si_codes
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToPrimitive)]
#[allow(dead_code)]
pub enum SigChildCode {
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

impl Into<i32> for SigChildCode {
    fn into(self) -> i32 {
        self as i32
    }
}

/// 信号默认处理函数——终止进程
fn sig_terminate(sig: Signal) {
    ProcessManager::exit(sig as usize);
}

/// 信号默认处理函数——终止进程并生成 core dump
fn sig_terminate_dump(sig: Signal) {
    ProcessManager::exit(sig as usize);
    // TODO 生成 coredump 文件
}

/// 信号默认处理函数——暂停进程
fn sig_stop(sig: Signal) {
    let guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
    ProcessManager::mark_stop().unwrap_or_else(|e| {
        kerror!(
            "sleep error :{:?},failed to sleep process :{:?}, with signal :{:?}",
            e,
            ProcessManager::current_pcb(),
            sig
        );
    });
    drop(guard);
    sched();
    // TODO 暂停进程
}

/// 信号默认处理函数——继续进程
fn sig_continue(sig: Signal) {
    ProcessManager::wakeup_stop(&ProcessManager::current_pcb()).unwrap_or_else(|_| {
        kerror!(
            "Failed to wake up process pid = {:?} with signal :{:?}",
            ProcessManager::current_pcb().pid(),
            sig
        );
    });
}
/// 信号默认处理函数——忽略
fn sig_ignore(_sig: Signal) {
    return;
}
