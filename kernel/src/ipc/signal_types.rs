use core::{ffi::c_void, mem::size_of};

use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::{
        asm::bitops::ffz,
        interrupt::TrapFrame,
        ipc::signal::{SigFlags, SigSet, Signal, MAX_SIG_NUM},
    },
    mm::VirtAddr,
    process::RawPid,
    syscall::user_access::UserBufferWriter,
};

/// siginfo中的si_code的可选值
/// 请注意，当这个值小于0时，表示siginfo来自用户态，否则来自内核态
#[derive(Copy, Debug, Clone, PartialEq, Eq)]
pub enum SigCode {
    /// sent by kill, sigsend, raise
    User = 0,
    /// queued SIGIO/POLL_IN
    PollIn = 1,
    /// queued SIGIO/POLL_OUT
    PollOut = 2,
    /// queued SIGIO/POLL_MSG
    PollMsg = 3,
    /// queued SIGIO/POLL_ERR
    PollErr = 4,
    /// queued SIGIO/POLL_PRI
    PollPri = 5,
    /// queued SIGIO/POLL_HUP
    PollHup = 6,
    /// sent by kernel from somewhere
    Kernel,
    /// SIGSYS sent by seccomp filter action SECCOMP_RET_TRAP.
    SysSeccomp,
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
    /// sent by tgkill/tkill
    Tkill = -6,
}

impl SigCode {
    pub fn as_i32(self) -> i32 {
        match self {
            Self::User => 0,
            Self::PollIn => 1,
            Self::PollOut => 2,
            Self::PollMsg => 3,
            Self::PollErr => 4,
            Self::PollPri => 5,
            Self::PollHup => 6,
            Self::Kernel => 0x80,
            Self::SysSeccomp => 1,
            Self::Queue => -1,
            Self::Timer => -2,
            Self::Mesgq => -3,
            Self::AsyncIO => -4,
            Self::SigIO => -5,
            Self::Tkill => -6,
        }
    }

    pub fn try_from_i32(x: i32) -> Option<SigCode> {
        match x {
            0 => Some(Self::User),
            1 => Some(Self::PollIn),
            2 => Some(Self::PollOut),
            3 => Some(Self::PollMsg),
            4 => Some(Self::PollErr),
            5 => Some(Self::PollPri),
            6 => Some(Self::PollHup),
            0x80 => Some(Self::Kernel),
            -1 => Some(Self::Queue),
            -2 => Some(Self::Timer),
            -3 => Some(Self::Mesgq),
            -4 => Some(Self::AsyncIO),
            -5 => Some(Self::SigIO),
            -6 => Some(Self::Tkill),
            _ => None,
        }
    }
}

/// 用户态程序传入的SIG_DFL的值
pub const USER_SIG_DFL: u64 = 0;
/// 用户态程序传入的SIG_IGN的值
pub const USER_SIG_IGN: u64 = 1;
/// 用户态程序传入的SIG_ERR的值
pub const USER_SIG_ERR: u64 = 2;

// 因为 Rust 编译器不能在常量声明中正确识别级联的 "|" 运算符(experimental feature： https://github.com/rust-lang/rust/issues/67792)，因此
// 暂时只能通过这种方法来声明这些常量，这些常量暂时没有全部用到，但是都出现在 linux 的判断逻辑中，所以都保留下来了
pub const SIG_KERNEL_ONLY_MASK: SigSet =
    Signal::into_sigset(Signal::SIGSTOP).union(Signal::into_sigset(Signal::SIGKILL));

pub const SIG_KERNEL_STOP_MASK: SigSet = Signal::into_sigset(Signal::SIGSTOP)
    .union(Signal::into_sigset(Signal::SIGTSTP))
    .union(Signal::into_sigset(Signal::SIGTTIN))
    .union(Signal::into_sigset(Signal::SIGTTOU));
#[allow(dead_code)]
pub const SIG_KERNEL_COREDUMP_MASK: SigSet = Signal::into_sigset(Signal::SIGQUIT)
    .union(Signal::into_sigset(Signal::SIGILL))
    .union(Signal::into_sigset(Signal::SIGTRAP))
    .union(Signal::into_sigset(Signal::SIGABRT_OR_IOT))
    .union(Signal::into_sigset(Signal::SIGFPE))
    .union(Signal::into_sigset(Signal::SIGSEGV))
    .union(Signal::into_sigset(Signal::SIGBUS))
    .union(Signal::into_sigset(Signal::SIGSYS))
    .union(Signal::into_sigset(Signal::SIGXCPU))
    .union(Signal::into_sigset(Signal::SIGXFSZ));

pub const SIG_KERNEL_IGNORE_MASK: SigSet = Signal::into_sigset(Signal::SIGCONT)
    .union(Signal::into_sigset(Signal::SIGCHLD))
    .union(Signal::into_sigset(Signal::SIGWINCH))
    .union(Signal::into_sigset(Signal::SIGURG));
#[allow(dead_code)]
pub const SIG_SPECIFIC_SICODES_MASK: SigSet = Signal::into_sigset(Signal::SIGILL)
    .union(Signal::into_sigset(Signal::SIGFPE))
    .union(Signal::into_sigset(Signal::SIGSEGV))
    .union(Signal::into_sigset(Signal::SIGBUS))
    .union(Signal::into_sigset(Signal::SIGTRAP))
    .union(Signal::into_sigset(Signal::SIGCHLD))
    .union(Signal::into_sigset(Signal::SIGIO_OR_POLL))
    .union(Signal::into_sigset(Signal::SIGSYS));

// Removed SignalStruct; refcount moved into Sighand

#[derive(Debug, Copy, Clone)]
#[allow(dead_code)]
pub enum SigactionType {
    SaHandler(SaHandlerType),
    SaSigaction(
        Option<
            unsafe extern "C" fn(
                sig: ::core::ffi::c_int,
                sinfo: *mut SigInfo,
                arg1: *mut ::core::ffi::c_void,
            ),
        >,
    ), // 暂时没有用上
}

impl SigactionType {
    /// Returns `true` if the sa handler type is [`Self::SaHandler(SaHandlerType::Default)`].
    ///
    /// [`SigDefault`]: SaHandlerType::SigDefault
    pub fn is_default(&self) -> bool {
        return matches!(self, Self::SaHandler(SaHandlerType::Default));
    }
    /// Returns `true` if the sa handler type is [`SaHandler(SaHandlerType::SigIgnore)`].
    ///
    /// [`SigIgnore`]: SaHandlerType::SigIgnore
    pub fn is_ignore(&self) -> bool {
        return matches!(self, Self::SaHandler(SaHandlerType::Ignore));
    }
    /// Returns `true` if the sa handler type is [`SaHandler(SaHandlerType::SigCustomized(_))`].
    ///
    /// [`SigCustomized`]: SaHandlerType::SigCustomized(_)
    pub fn is_customized(&self) -> bool {
        return matches!(self, Self::SaHandler(SaHandlerType::Customized(_)));
    }
}

#[derive(Debug, Copy, Clone)]
#[allow(dead_code)]
pub enum SaHandlerType {
    Error, // 暂时没有用上
    Default,
    Ignore,
    Customized(VirtAddr),
}

impl From<SaHandlerType> for usize {
    fn from(value: SaHandlerType) -> Self {
        match value {
            SaHandlerType::Error => 2,
            SaHandlerType::Ignore => 1,
            SaHandlerType::Default => 0,
            SaHandlerType::Customized(handler) => handler.data(),
        }
    }
}

impl SaHandlerType {
    /// Returns `true` if the sa handler type is [`SigDefault`].
    ///
    /// [`SigDefault`]: SaHandlerType::SigDefault
    pub fn is_sig_default(&self) -> bool {
        matches!(self, Self::Default)
    }

    /// Returns `true` if the sa handler type is [`SigIgnore`].
    ///
    /// [`SigIgnore`]: SaHandlerType::SigIgnore
    pub fn is_sig_ignore(&self) -> bool {
        matches!(self, Self::Ignore)
    }

    /// Returns `true` if the sa handler type is [`SigError`].
    ///
    /// [`SigError`]: SaHandlerType::SigError
    pub fn is_sig_error(&self) -> bool {
        matches!(self, Self::Error)
    }
}

/// 信号处理结构体
///
#[derive(Debug, Copy, Clone)]
pub struct Sigaction {
    action: SigactionType,
    flags: SigFlags,
    mask: SigSet, // 为了可扩展性而设置的sa_mask
    /// 信号处理函数执行结束后，将会跳转到这个函数内进行执行，然后执行sigreturn系统调用
    restorer: Option<VirtAddr>,
}

impl Default for Sigaction {
    fn default() -> Self {
        Self {
            action: SigactionType::SaHandler(SaHandlerType::Default),
            flags: Default::default(),
            mask: Default::default(),
            restorer: Default::default(),
        }
    }
}

impl Sigaction {
    /// 判断传入的信号是否被设置为默认处理
    pub fn is_default(&self) -> bool {
        return self.action.is_default();
    }
    /// 判断传入的信号是否被忽略
    pub fn is_ignore(&self) -> bool {
        return self.action.is_ignore();
    }

    pub fn new(
        action: SigactionType,
        flags: SigFlags,
        mask: SigSet,
        restorer: Option<VirtAddr>,
    ) -> Self {
        Self {
            action,
            flags,
            mask,
            restorer,
        }
    }

    pub fn action(&self) -> SigactionType {
        self.action
    }

    pub fn flags(&self) -> SigFlags {
        self.flags
    }

    pub fn restorer(&self) -> Option<VirtAddr> {
        self.restorer
    }

    pub fn flags_mut(&mut self) -> &mut SigFlags {
        &mut self.flags
    }

    pub fn set_action(&mut self, action: SigactionType) {
        self.action = action;
    }

    pub fn mask(&self) -> SigSet {
        self.mask
    }

    pub fn mask_mut(&mut self) -> &mut SigSet {
        &mut self.mask
    }

    pub fn set_restorer(&mut self, restorer: Option<VirtAddr>) {
        self.restorer = restorer;
    }

    /// 默认信号处理程序占位符（用于在sighand结构体中的action数组中占位）
    pub const DEFAULT_SIGACTION: Sigaction = Sigaction {
        action: SigactionType::SaHandler(SaHandlerType::Default),
        flags: SigFlags::empty(),
        mask: SigSet::from_bits_truncate(0),
        restorer: None,
    };

    /// 默认的“忽略信号”的sigaction
    pub const DEFAULT_SIGACTION_IGNORE: Sigaction = Sigaction {
        action: SigactionType::SaHandler(SaHandlerType::Ignore),
        flags: SigFlags::empty(),
        mask: SigSet::from_bits_truncate(0),
        restorer: None,
    };
}

/// 用户态传入的sigaction结构体（符合posix规范）
/// 请注意，我们会在sys_sigaction函数里面将其转换成内核使用的sigaction结构体
#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct UserSigaction {
    pub handler: *mut core::ffi::c_void,
    pub flags: SigFlags,
    pub restorer: *mut core::ffi::c_void,
    pub mask: SigSet,
}

/**
 * 内核内部使用的SigInfo结构体，不直接暴露给用户态
 * 用于内核内部的信号信息存储和处理
 */
#[derive(Copy, Clone, Debug)]
pub struct SigInfo {
    sig_no: i32,
    errno: i32,
    sig_code: SigCode,
    sig_type: SigType,
}

/**
 * 标准POSIX siginfo_t结构体，用于用户态接口
 * 完全兼容Linux标准，大小为128字节
 *
 * 字段顺序必须严格按照Linux标准：si_signo, si_errno, si_code
 */
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct PosixSigInfo {
    pub si_signo: i32,
    pub si_errno: i32,
    pub si_code: i32,
    pub _sifields: PosixSiginfoFields,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union PosixSiginfoFields {
    pub _kill: PosixSiginfoKill,
    pub _timer: PosixSiginfoTimer,
    pub _rt: PosixSiginfoRt,
    pub _sigchld: PosixSiginfoSigchld,
    pub _sigfault: PosixSiginfoSigfault,
    pub _sigpoll: PosixSiginfoSigpoll,
    pub _sigsys: PosixSiginfoSigsys,
    // 填充到128字节
    _pad: [u8; 128 - 16],
}

// 编译期校验：确保 PosixSigInfo 与 Linux 的 siginfo_t 大小一致（128 字节）
const _: [(); 128] = [(); core::mem::size_of::<PosixSigInfo>()];

impl core::fmt::Debug for PosixSiginfoFields {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // 由于是联合体，我们只显示_kill字段作为默认表示
        f.debug_struct("PosixSiginfoFields")
            .field("_kill", unsafe { &self._kill })
            .finish()
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct PosixSiginfoKill {
    pub si_pid: i32,
    pub si_uid: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct PosixSiginfoTimer {
    pub si_tid: i32,
    pub si_overrun: i32,
    pub si_sigval: PosixSigval,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct PosixSiginfoRt {
    pub si_pid: i32,
    pub si_uid: u32,
    pub si_sigval: PosixSigval,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct PosixSiginfoSigchld {
    pub si_pid: i32,
    pub si_uid: u32,
    pub si_status: i32,
    pub si_utime: i64,
    pub si_stime: i64,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct PosixSiginfoSigfault {
    pub si_addr: u64,
    pub si_addr_lsb: u16,
    pub si_band: i32,
    pub si_fd: i32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct PosixSiginfoSigpoll {
    pub si_band: i64,
    pub si_fd: i32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct PosixSiginfoSigsys {
    pub _call_addr: u64,
    pub _syscall: i32,
    pub _arch: u32,
}

/// 标准 POSIX sigval_t（union）。
///
/// 用户态会通过 `si_int` / `si_ptr` 访问同一片内存，因此必须是 union，且大小应为 8 字节。
#[repr(C)]
#[derive(Copy, Clone)]
pub union PosixSigval {
    pub sival_int: i32,
    pub sival_ptr: u64,
}

impl PosixSigval {
    #[inline(always)]
    pub const fn from_int(v: i32) -> Self {
        Self { sival_int: v }
    }

    #[inline(always)]
    pub const fn from_ptr(v: u64) -> Self {
        Self { sival_ptr: v }
    }

    #[inline(always)]
    pub const fn zero() -> Self {
        Self { sival_ptr: 0 }
    }
}

impl core::fmt::Debug for PosixSigval {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // union：同时以 int/ptr 两种视角打印，便于调试
        let as_int = unsafe { self.sival_int };
        let as_ptr = unsafe { self.sival_ptr };
        f.debug_struct("PosixSigval")
            .field("sival_int", &as_int)
            .field("sival_ptr", &as_ptr)
            .finish()
    }
}

// 编译期校验：sigval_t 在 64-bit 架构下应为 8 字节
const _: [(); 8] = [(); core::mem::size_of::<PosixSigval>()];

impl SigInfo {
    pub fn sig_code(&self) -> SigCode {
        self.sig_code
    }

    #[inline(always)]
    pub fn signo_i32(&self) -> i32 {
        self.sig_no
    }

    #[inline(always)]
    pub fn is_signal(&self, sig: Signal) -> bool {
        self.sig_no == sig as i32
    }

    pub fn set_sig_type(&mut self, sig_type: SigType) {
        self.sig_type = sig_type;
    }

    /// 若该 SigInfo 为指定 timerid 的 POSIX timer 信号，则将其 si_overrun 增加 bump，并返回 true。
    pub fn bump_posix_timer_overrun(&mut self, timerid: i32, bump: i32) -> bool {
        match self.sig_type {
            SigType::PosixTimer {
                timerid: tid,
                overrun,
                sigval,
            } if tid == timerid => {
                let new_overrun = overrun.saturating_add(bump);
                self.sig_type = SigType::PosixTimer {
                    timerid: tid,
                    overrun: new_overrun,
                    sigval,
                };
                true
            }
            _ => false,
        }
    }

    /// 若该 SigInfo 为指定 timerid 的 POSIX timer 信号，则将其 si_overrun 重置为 0，并返回 true。
    pub fn reset_posix_timer_overrun(&mut self, timerid: i32) -> bool {
        match self.sig_type {
            SigType::PosixTimer {
                timerid: tid,
                overrun: _,
                sigval,
            } if tid == timerid => {
                self.sig_type = SigType::PosixTimer {
                    timerid: tid,
                    overrun: 0,
                    sigval,
                };
                true
            }
            _ => false,
        }
    }

    /// 将内核SigInfo转换为标准PosixSigInfo
    #[inline(never)]
    pub fn convert_to_posix_siginfo(&self) -> PosixSigInfo {
        match self.sig_type {
            SigType::Kill { pid, uid } => PosixSigInfo {
                si_signo: self.sig_no,
                si_errno: self.errno,
                si_code: self.sig_code.as_i32(),
                _sifields: PosixSiginfoFields {
                    _kill: PosixSiginfoKill {
                        si_pid: pid.data() as i32,
                        si_uid: uid,
                    },
                },
            },
            SigType::Rt { pid, uid, sigval } => PosixSigInfo {
                si_signo: self.sig_no,
                si_errno: self.errno,
                si_code: self.sig_code.as_i32(),
                _sifields: PosixSiginfoFields {
                    _rt: PosixSiginfoRt {
                        si_pid: pid.data() as i32,
                        si_uid: uid,
                        si_sigval: sigval,
                    },
                },
            },
            SigType::Alarm(pid) => PosixSigInfo {
                si_signo: self.sig_no,
                si_errno: self.errno,
                si_code: self.sig_code.as_i32(),
                _sifields: PosixSiginfoFields {
                    _timer: PosixSiginfoTimer {
                        si_tid: pid.data() as i32,
                        si_overrun: 0,
                        si_sigval: PosixSigval::zero(),
                    },
                },
            },
            SigType::PosixTimer {
                timerid,
                overrun,
                sigval,
            } => PosixSigInfo {
                si_signo: self.sig_no,
                si_errno: self.errno,
                si_code: self.sig_code.as_i32(),
                _sifields: PosixSiginfoFields {
                    _timer: PosixSiginfoTimer {
                        si_tid: timerid,
                        si_overrun: overrun,
                        si_sigval: sigval,
                    },
                },
            },
            SigType::SigPoll { fd, band } => PosixSigInfo {
                si_signo: self.sig_no,
                si_errno: self.errno,
                si_code: self.sig_code.as_i32(),
                _sifields: PosixSiginfoFields {
                    _sigpoll: PosixSiginfoSigpoll {
                        si_band: band,
                        si_fd: fd,
                    },
                },
            },
            SigType::SigSys {
                call_addr,
                syscall,
                arch,
            } => PosixSigInfo {
                si_signo: self.sig_no,
                si_errno: self.errno,
                si_code: self.sig_code.as_i32(),
                _sifields: PosixSiginfoFields {
                    _sigsys: PosixSiginfoSigsys {
                        _call_addr: call_addr,
                        _syscall: syscall,
                        _arch: arch,
                    },
                },
            },
        }
    }

    /// @brief 将PosixSigInfo结构体拷贝到用户栈
    /// ## 参数
    ///
    /// `to` 用户空间指针
    ///
    /// ## 注意
    ///
    /// 该函数将内核SigInfo转换为标准PosixSigInfo后拷贝到用户态
    ///
    /// 该函数对应Linux中的https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/signal.c#3323
    #[inline(never)]
    pub fn copy_posix_siginfo_to_user(&self, to: *mut PosixSigInfo) -> Result<i32, SystemError> {
        // 验证目标地址是否为用户空间
        let posix_siginfo = self.convert_to_posix_siginfo();
        let mut user_buffer = UserBufferWriter::new(to, size_of::<PosixSigInfo>(), true)?;

        let retval: Result<i32, SystemError> = Ok(0);

        user_buffer.copy_one_to_user(&posix_siginfo, 0)?;
        return retval;
    }
}

#[derive(Copy, Clone, Debug)]
pub enum SigType {
    /// kill/tgkill/tkill 等用户态发起的信号：携带发送者 pid/uid。
    Kill {
        pid: RawPid,
        uid: u32,
    },
    /// SI_QUEUE 语义（rt_sigqueueinfo/sigqueue）：携带发送者 pid/uid 与 sigval。
    Rt {
        pid: RawPid,
        uid: u32,
        sigval: PosixSigval,
    },
    Alarm(RawPid),
    /// POSIX interval timer 发送的信号（SI_TIMER）。
    /// - `timerid`: 对应用户态 `siginfo_t::si_timerid`
    /// - `overrun`: 对应用户态 `siginfo_t::si_overrun`
    /// - `sigval`: 对应用户态 `siginfo_t::si_value`
    PosixTimer {
        timerid: i32,
        overrun: i32,
        sigval: PosixSigval,
    },
    /// queued SIGIO/F_SETSIG signal carrying poll band and fd.
    SigPoll {
        fd: i32,
        band: i64,
    },
    /// SIGSYS/SYS_SECCOMP seccomp trap siginfo payload.
    SigSys {
        call_addr: u64,
        syscall: i32,
        arch: u32,
    },
    // 后续完善下列中的具体字段
    // Timer,
    // Rt,
    // SigChild,
    // SigFault,
    // SigSys,
}

impl SigInfo {
    pub fn new(sig: Signal, sig_errno: i32, sig_code: SigCode, sig_type: SigType) -> Self {
        Self {
            sig_no: sig as i32,
            sig_code,
            errno: sig_errno,
            sig_type,
        }
    }
}

#[derive(Debug, Default)]
pub struct SigPending {
    signal: SigSet,
    queue: SigQueue,
}

impl SigPending {
    /// 判断是否有待处理的信号
    pub fn has_pending(&self) -> bool {
        return !self.signal.is_empty();
    }

    pub fn signal(&self) -> SigSet {
        self.signal
    }

    pub fn queue(&self) -> &SigQueue {
        &self.queue
    }

    pub fn queue_mut(&mut self) -> &mut SigQueue {
        &mut self.queue
    }

    /// 在当前线程 pending 队列中判断是否已存在指定 timerid 的 POSIX timer 信号。
    pub fn posix_timer_exists(&mut self, sig: Signal, timerid: i32) -> bool {
        for info in self.queue.q.iter_mut() {
            // bump(0) 作为“匹配探测”，不会改变值
            if info.is_signal(sig)
                && info.sig_code() == SigCode::Timer
                && info.bump_posix_timer_overrun(timerid, 0)
            {
                return true;
            }
        }
        false
    }

    /// 若当前线程 pending 队列中已存在该 timer 的信号，则将其 si_overrun 增加 bump，并返回 true。
    pub fn posix_timer_bump_overrun(&mut self, sig: Signal, timerid: i32, bump: i32) -> bool {
        for info in self.queue.q.iter_mut() {
            if info.is_signal(sig)
                && info.sig_code() == SigCode::Timer
                && info.bump_posix_timer_overrun(timerid, bump)
            {
                return true;
            }
        }
        false
    }

    /// 将当前线程 pending 队列中属于该 timer 的信号的 si_overrun 重置为 0（若找到则返回 true）。
    pub fn posix_timer_reset_overrun(&mut self, sig: Signal, timerid: i32) -> bool {
        for info in self.queue.q.iter_mut() {
            if info.is_signal(sig)
                && info.sig_code() == SigCode::Timer
                && info.reset_posix_timer_overrun(timerid)
            {
                return true;
            }
        }
        false
    }

    pub fn signal_mut(&mut self) -> &mut SigSet {
        &mut self.signal
    }
    /// @brief 获取下一个要处理的信号（sig number越小的信号，优先级越高）
    ///
    /// @param pending 等待处理的信号
    /// @param sig_mask 屏蔽了的信号
    /// @return i32 下一个要处理的信号的number. 如果为0,则无效
    pub fn next_signal(&self, sig_mask: &SigSet) -> Signal {
        let mut sig = Signal::INVALID;

        let s = self.signal();
        let m = *sig_mask;
        m.is_empty();
        // 获取第一个待处理的信号的号码
        let x = s & (!m);
        if x.bits() != 0 {
            sig = Signal::from(ffz(x.complement().bits()) + 1);
            return sig;
        }

        // 暂时只支持64种信号
        assert_eq!(MAX_SIG_NUM, 64);

        return sig;
    }
    /// @brief 收集信号的信息
    ///
    /// @param sig 要收集的信号的信息
    /// @param pending 信号的排队等待标志
    /// @return SigInfo 信号的信息
    pub fn collect_signal(&mut self, sig: Signal) -> SigInfo {
        let (info, still_pending) = self.queue_mut().find_and_delete(sig);

        // 如果没有仍在等待的信号，则清除pending位
        if !still_pending {
            self.signal_mut().remove(sig.into());
        }

        if let Some(info) = info {
            return info;
        } else {
            // 信号不在sigqueue中，这意味着当前信号是来自快速路径，因此直接把siginfo设置为0即可。
            let mut ret = SigInfo::new(
                sig,
                0,
                SigCode::User,
                SigType::Kill {
                    pid: RawPid::from(0),
                    uid: 0,
                },
            );
            ret.set_sig_type(SigType::Kill {
                pid: RawPid::new(0),
                uid: 0,
            });
            return ret;
        }
    }

    /// @brief 从当前进程的sigpending中取出下一个待处理的signal，并返回给调用者。（调用者应当处理这个信号）
    /// 请注意，进入本函数前，当前进程应当持有current_pcb().sighand.siglock
    pub fn dequeue_signal(&mut self, sig_mask: &SigSet) -> (Signal, Option<SigInfo>) {
        // 获取下一个要处理的信号的编号
        let sig = self.next_signal(sig_mask);

        let info: Option<SigInfo> = if sig != Signal::INVALID {
            // 如果下一个要处理的信号是合法的，则收集其siginfo
            Some(self.collect_signal(sig))
        } else {
            None
        };

        return (sig, info);
    }
    /// @brief 从sigpending中删除mask中被置位的信号。也就是说，比如mask的第1位被置为1,那么就从sigqueue中删除所有signum为2的信号的信息。
    pub fn flush_by_mask(&mut self, mask: &SigSet) {
        // 定义过滤器，从sigqueue中删除mask中被置位的信号
        let filter = |x: &SigInfo| !mask.contains(Signal::from(x.sig_no as usize).into());
        self.queue.q.retain(filter);
        // 同步清理位图中的相应位，避免仅删除队列项但仍因位图残留被视为pending
        self.signal.remove(*mask);
    }
}

/// @brief 进程接收到的信号的队列
#[derive(Debug, Clone, Default)]
pub struct SigQueue {
    pub q: Vec<SigInfo>,
}

#[allow(dead_code)]
impl SigQueue {
    /// @brief 初始化一个新的信号队列
    pub fn new(capacity: usize) -> Self {
        SigQueue {
            q: Vec::with_capacity(capacity),
        }
    }

    /// @brief 在信号队列中寻找第一个满足要求的siginfo, 并返回它的引用
    ///
    /// @return (第一个满足要求的siginfo的引用; 是否有多个满足条件的siginfo)
    pub fn find(&self, sig: Signal) -> (Option<&SigInfo>, bool) {
        // 是否存在多个满足条件的siginfo
        let mut still_pending = false;
        let mut info: Option<&SigInfo> = None;

        for x in self.q.iter() {
            if x.sig_no == sig as i32 {
                if info.is_some() {
                    still_pending = true;
                    break;
                } else {
                    info = Some(x);
                }
            }
        }
        return (info, still_pending);
    }

    /// @brief 在信号队列中寻找第一个满足要求的siginfo, 并将其从队列中删除，然后返回这个siginfo
    ///
    /// @return (第一个满足要求的siginfo; 从队列中删除前是否有多个满足条件的siginfo)
    pub fn find_and_delete(&mut self, sig: Signal) -> (Option<SigInfo>, bool) {
        // 是否存在多个满足条件的siginfo
        let mut still_pending = false;
        let mut first = true; // 标记变量，记录当前是否已经筛选出了一个元素

        let filter = |x: &mut SigInfo| {
            if x.sig_no == sig as i32 {
                if !first {
                    // 如果之前已经筛选出了一个元素，则不把当前元素删除
                    still_pending = true;
                    return false;
                } else {
                    // 当前是第一个被筛选出来的元素
                    first = false;
                    return true;
                }
            }
            return false;
        };
        // 从sigqueue中过滤出结果
        let mut filter_result: Vec<SigInfo> = self.q.extract_if(.., filter).collect();
        // 筛选出的结果不能大于1个
        assert!(filter_result.len() <= 1);

        return (filter_result.pop(), still_pending);
    }

    /// @brief 从C的void*指针转换为static生命周期的可变引用
    pub fn from_c_void(p: *mut c_void) -> &'static mut SigQueue {
        let sq = p as *mut SigQueue;
        let sq = unsafe { sq.as_mut::<'static>() }.unwrap();
        return sq;
    }
}

///
/// 定义了不同架构下实现 Signal 要实现的接口
///
pub trait SignalArch {
    /// 信号处理函数
    ///
    /// 处理信号或重启系统调用
    ///
    /// ## 参数
    ///
    /// - `frame` 中断栈帧
    unsafe fn do_signal_or_restart(frame: &mut TrapFrame);

    fn sys_rt_sigreturn(trap_frame: &mut TrapFrame) -> u64;
}

bitflags! {

    /// https://code.dragonos.org.cn/xref/linux-6.6.21/include/linux/sched/signal.h#253
    pub struct SignalFlags: u32 {
        const STOP_STOPPED = 0x00000001; /* job control stop in effect */
        const STOP_CONTINUED = 0x00000002; /* SIGCONT since WCONTINUED reap */
        const GROUP_EXIT = 0x00000004; /* group exit in progress */
        const GROUP_EXEC = 0x00000008; /* exec in progress (de-threading) */
        const CLD_STOPPED = 0x00000010; /* Pending notifications to parent */
        const CLD_CONTINUED = 0x00000020;
        const UNKILLABLE = 0x00000040; /* for init: ignore fatal signals */
    }
}

impl SignalFlags {
    pub const CLD_MASK: SignalFlags = SignalFlags::CLD_STOPPED.union(SignalFlags::CLD_CONTINUED);
    pub const STOP_MASK: SignalFlags = SignalFlags::CLD_MASK
        .union(SignalFlags::STOP_STOPPED)
        .union(SignalFlags::STOP_CONTINUED);
}
