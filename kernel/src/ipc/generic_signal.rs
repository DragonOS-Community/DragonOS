use core::sync::atomic::compiler_fence;

use num_traits::FromPrimitive;

use crate::ipc::signal_types::SignalFlags;
use crate::{
    arch::{
        ipc::signal::{SigFlags, SigSet, Signal, MAX_SIG_NUM},
        CurrentIrqArch,
    },
    exception::InterruptArch,
    process::{ProcessFlags, ProcessManager},
    sched::{schedule, SchedMode},
};
use alloc::sync::Arc;

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
    // 实时信号：SIGRTMIN+1 到 SIGRTMAX-1
    SIGRTMIN_1 = 33,
    SIGRTMIN_2 = 34,
    SIGRTMIN_3 = 35,
    SIGRTMIN_4 = 36,
    SIGRTMIN_5 = 37,
    SIGRTMIN_6 = 38,
    SIGRTMIN_7 = 39,
    SIGRTMIN_8 = 40,
    SIGRTMIN_9 = 41,
    SIGRTMIN_10 = 42,
    SIGRTMIN_11 = 43,
    SIGRTMIN_12 = 44,
    SIGRTMIN_13 = 45,
    SIGRTMIN_14 = 46,
    SIGRTMIN_15 = 47,
    SIGRTMIN_16 = 48,
    SIGRTMIN_17 = 49,
    SIGRTMIN_18 = 50,
    SIGRTMIN_19 = 51,
    SIGRTMIN_20 = 52,
    SIGRTMIN_21 = 53,
    SIGRTMIN_22 = 54,
    SIGRTMIN_23 = 55,
    SIGRTMIN_24 = 56,
    SIGRTMIN_25 = 57,
    SIGRTMIN_26 = 58,
    SIGRTMIN_27 = 59,
    SIGRTMIN_28 = 60,
    SIGRTMIN_29 = 61,
    SIGRTMIN_30 = 62,
    SIGRTMIN_31 = 63,
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

    /// 判断一个信号号是否为实时信号
    #[inline]
    pub fn is_rt_signal_number(sig_num: i32) -> bool {
        sig_num >= Self::SIGRTMIN as i32 && sig_num <= Self::SIGRTMAX as i32
    }

    /// 获取实时信号的范围
    #[inline]
    pub fn rt_signal_range() -> (i32, i32) {
        (Self::SIGRTMIN as i32, Self::SIGRTMAX as i32)
    }

    /// 调用信号的默认处理函数
    pub fn handle_default(&self) {
        match self {
            Self::INVALID => {
                log::error!("attempting to handler an Invalid");
            }
            Self::SIGHUP => sig_terminate(*self),
            Self::SIGINT => sig_terminate(*self),
            Self::SIGQUIT => sig_terminate_dump(*self),
            Self::SIGILL => sig_terminate_dump(*self),
            Self::SIGTRAP => sig_terminate_dump(*self),
            Self::SIGABRT_OR_IOT => sig_terminate_dump(*self),
            Self::SIGBUS => sig_terminate_dump(*self),
            Self::SIGFPE => sig_terminate_dump(*self),
            Self::SIGKILL => sig_terminate(*self),
            Self::SIGUSR1 => sig_terminate(*self),
            Self::SIGSEGV => sig_terminate_dump(*self),
            Self::SIGUSR2 => sig_terminate(*self),
            Self::SIGPIPE => sig_terminate(*self),
            Self::SIGALRM => sig_terminate(*self),
            Self::SIGTERM => sig_terminate(*self),
            Self::SIGSTKFLT => sig_terminate(*self),
            Self::SIGCHLD => sig_ignore(*self),
            Self::SIGCONT => sig_continue(*self),
            Self::SIGSTOP => sig_stop(*self),
            Self::SIGTSTP => sig_stop(*self),
            Self::SIGTTIN => sig_stop(*self),
            Self::SIGTTOU => sig_stop(*self),
            Self::SIGURG => sig_ignore(*self),
            Self::SIGXCPU => sig_terminate_dump(*self),
            Self::SIGXFSZ => sig_terminate_dump(*self),
            Self::SIGVTALRM => sig_terminate(*self),
            Self::SIGPROF => sig_terminate(*self),
            Self::SIGWINCH => sig_ignore(*self),
            Self::SIGIO_OR_POLL => sig_terminate(*self),
            Self::SIGPWR => sig_terminate(*self),
            Self::SIGSYS => sig_terminate(*self),
            // 实时信号默认处理：终止进程
            Self::SIGRTMIN => sig_terminate(*self),
            Self::SIGRTMIN_1 => sig_terminate(*self),
            Self::SIGRTMIN_2 => sig_terminate(*self),
            Self::SIGRTMIN_3 => sig_terminate(*self),
            Self::SIGRTMIN_4 => sig_terminate(*self),
            Self::SIGRTMIN_5 => sig_terminate(*self),
            Self::SIGRTMIN_6 => sig_terminate(*self),
            Self::SIGRTMIN_7 => sig_terminate(*self),
            Self::SIGRTMIN_8 => sig_terminate(*self),
            Self::SIGRTMIN_9 => sig_terminate(*self),
            Self::SIGRTMIN_10 => sig_terminate(*self),
            Self::SIGRTMIN_11 => sig_terminate(*self),
            Self::SIGRTMIN_12 => sig_terminate(*self),
            Self::SIGRTMIN_13 => sig_terminate(*self),
            Self::SIGRTMIN_14 => sig_terminate(*self),
            Self::SIGRTMIN_15 => sig_terminate(*self),
            Self::SIGRTMIN_16 => sig_terminate(*self),
            Self::SIGRTMIN_17 => sig_terminate(*self),
            Self::SIGRTMIN_18 => sig_terminate(*self),
            Self::SIGRTMIN_19 => sig_terminate(*self),
            Self::SIGRTMIN_20 => sig_terminate(*self),
            Self::SIGRTMIN_21 => sig_terminate(*self),
            Self::SIGRTMIN_22 => sig_terminate(*self),
            Self::SIGRTMIN_23 => sig_terminate(*self),
            Self::SIGRTMIN_24 => sig_terminate(*self),
            Self::SIGRTMIN_25 => sig_terminate(*self),
            Self::SIGRTMIN_26 => sig_terminate(*self),
            Self::SIGRTMIN_27 => sig_terminate(*self),
            Self::SIGRTMIN_28 => sig_terminate(*self),
            Self::SIGRTMIN_29 => sig_terminate(*self),
            Self::SIGRTMIN_30 => sig_terminate(*self),
            Self::SIGRTMIN_31 => sig_terminate(*self),
            Self::SIGRTMAX => sig_terminate(*self),
        }
    }

    pub fn kernel_only(&self) -> bool {
        crate::ipc::signal_types::SIG_KERNEL_ONLY_MASK.contains(self.into_sigset())
    }

    /// 判断信号的默认行为是否为忽略
    pub fn kernel_ignore(&self) -> bool {
        crate::ipc::signal_types::SIG_KERNEL_IGNORE_MASK.contains(self.into_sigset())
    }

    /// 判断信号的默认行为是否为停止进程
    pub fn kernel_stop(&self) -> bool {
        crate::ipc::signal_types::SIG_KERNEL_STOP_MASK.contains(self.into_sigset())
    }

    /// 判断信号的默认行为是否为 coredump
    pub fn kernel_coredump(&self) -> bool {
        crate::ipc::signal_types::SIG_KERNEL_COREDUMP_MASK.contains(self.into_sigset())
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
            log::error!(
                "Try to convert a negative number {} to GenericSignal",
                value
            );
            return GenericSignal::INVALID;
        } else if value as usize > GENERIC_MAX_SIG_NUM {
            log::error!(
                "Try to convert an out-of-range number {} to GenericSignal (max: {})",
                value,
                GENERIC_MAX_SIG_NUM
            );
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
        // 实时信号位图：SIGRTMIN+1 到 SIGRTMAX-1
        const SIGRTMIN_1 =  1<<32;
        const SIGRTMIN_2 =  1<<33;
        const SIGRTMIN_3 =  1<<34;
        const SIGRTMIN_4 =  1<<35;
        const SIGRTMIN_5 =  1<<36;
        const SIGRTMIN_6 =  1<<37;
        const SIGRTMIN_7 =  1<<38;
        const SIGRTMIN_8 =  1<<39;
        const SIGRTMIN_9 =  1<<40;
        const SIGRTMIN_10 = 1<<41;
        const SIGRTMIN_11 = 1<<42;
        const SIGRTMIN_12 = 1<<43;
        const SIGRTMIN_13 = 1<<44;
        const SIGRTMIN_14 = 1<<45;
        const SIGRTMIN_15 = 1<<46;
        const SIGRTMIN_16 = 1<<47;
        const SIGRTMIN_17 = 1<<48;
        const SIGRTMIN_18 = 1<<49;
        const SIGRTMIN_19 = 1<<50;
        const SIGRTMIN_20 = 1<<51;
        const SIGRTMIN_21 = 1<<52;
        const SIGRTMIN_22 = 1<<53;
        const SIGRTMIN_23 = 1<<54;
        const SIGRTMIN_24 = 1<<55;
        const SIGRTMIN_25 = 1<<56;
        const SIGRTMIN_26 = 1<<57;
        const SIGRTMIN_27 = 1<<58;
        const SIGRTMIN_28 = 1<<59;
        const SIGRTMIN_29 = 1<<60;
        const SIGRTMIN_30 = 1<<61;
        const SIGRTMIN_31 = 1<<62;
        const SIGRTMAX =  1<<63;
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

bitflags! {
    #[repr(C)]
    #[derive(Default)]
    pub struct GenericSigStackFlags:u32{
        const SS_ONSTACK = 1;
        const SS_DISABLE = 2;
        const SS_AUTODISARM = 1 << 31;
    }
}

/// 信号默认处理函数——终止进程
fn sig_terminate(sig: Signal) {
    if sig == Signal::SIGKILL {
        let current = ProcessManager::current_pcb();
        let sighand = current.sighand();
        if sighand.flags_contains(SignalFlags::GROUP_EXEC) {
            if let Some(exec_task) = sighand.group_exec_task() {
                if !Arc::ptr_eq(&exec_task, &current) {
                    ProcessManager::exit(sig as usize);
                }
            }
        }
    }
    let code = ProcessManager::current_pcb()
        .sighand()
        .group_exit_code_if_set();
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    // 若线程组退出已经在进行中，则所有后续致命信号都应当使用统一的 group_exit_code，
    // 避免多次触发 zap_other_threads 等逻辑。
    if let Some(code) = code {
        ProcessManager::exit(code);
    } else {
        // 还未进入 group-exit：按照 Linux 语义，
        // 第一个致命信号负责设置 group_exit_code 并终止整个线程组。
        //
        // 对于信号导致的退出，exit_code 不需要左移（低 7 位即为信号编号）。
        ProcessManager::group_exit(sig as usize);
    }
}

/// 信号默认处理函数——终止进程并生成 core dump
fn sig_terminate_dump(sig: Signal) {
    let code = ProcessManager::current_pcb()
        .sighand()
        .group_exit_code_if_set();
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    if let Some(code) = code {
        ProcessManager::exit(code);
    } else {
        // TODO: 未来在这里补充 coredump 逻辑；目前先复用 group_exit 语义
        ProcessManager::group_exit(sig as usize);
    }
    // TODO 生成 coredump 文件
}

/// 信号默认处理函数——暂停进程 (SIGSTOP/SIGTSTP/SIGTTIN/SIGTTOU)
///
fn sig_stop(sig: Signal) {
    let pcb = ProcessManager::current_pcb();

    // ===== Ptrace 进程的特殊处理 =====
    // 被 ptrace 的进程由 tracer 控制其状态(TASK_TRACED)，不进入标准的 TASK_STOPPED
    // 如果执行到这里，说明 ptrace_signal 已经在 do_signal 中处理过该信号
    // tracer 决定将信号注入给 tracee，但这不意味着 tracee 要再次停止
    // 直接返回，不做任何操作
    if pcb.flags().contains(ProcessFlags::PTRACED) {
        return;
    }

    // ===== 非 ptrace 进程的 Group Stop 逻辑 =====
    // 标记停止事件，供 waitid(WSTOPPED) 可见
    pcb.sighand().flags_insert(SignalFlags::CLD_STOPPED);
    pcb.sighand().flags_insert(SignalFlags::STOP_STOPPED);

    // 切换进程状态为 Stopped 并调度
    let guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
    ProcessManager::mark_stop(sig).unwrap_or_else(|e| {
        log::error!(
            "sleep error :{:?},failed to sleep process :{:?}, with signal :{:?}",
            e,
            pcb.pid(),
            sig
        );
    });
    drop(guard);

    // 向父进程报告 SIGCHLD 并唤醒父进程可能阻塞的 wait
    let pcb = ProcessManager::current_pcb();
    if let Some(parent) = pcb.parent_pcb() {
        // 检查父进程是否设置了 SA_NOCLDSTOP
        let should_notify = {
            let sighand = parent.sighand();
            sighand
                .handler(Signal::SIGCHLD)
                .map(|sa| !sa.flags().contains(SigFlags::SA_NOCLDSTOP))
                .unwrap_or(false)
        };

        if should_notify {
            let _ = crate::ipc::kill::send_signal_to_pcb(parent.clone(), Signal::SIGCHLD);
        }
        // 无论是否发送 SIGCHLD，都需要唤醒父进程的 wait 队列，因为 waitpid(WUNTRACED) 可能需要返回
        parent.wake_all_waiters();
    }
    // 唤醒等待在该子进程等待队列上的等待者
    pcb.wake_all_waiters();
    // 让出 CPU 进入睡眠
    schedule(SchedMode::SM_NONE);
}

/// 信号默认处理函数——继续进程
fn sig_continue(_sig: Signal) {
    // 默认处理改为最小化：仅在已处于 Stopped 时唤醒停止，让进程继续运行。
    let pcb = ProcessManager::current_pcb();
    let is_stopped = pcb
        .sched_info()
        .inner_lock_read_irqsave()
        .state()
        .is_stopped();
    if is_stopped {
        let _ = ProcessManager::wakeup_stop(&pcb);
    }
    // 标志位设置与父进程通知统一由 prepare_signal(SIGCONT) 路径处理，避免重复/竞态
}

/// 信号默认处理函数——忽略
fn sig_ignore(_sig: Signal) {
    return;
}
