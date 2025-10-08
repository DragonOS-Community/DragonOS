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
    /// sent by tgkill/tkill
    Tkill = -6,
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
            -6 => Self::Tkill,
            _ => panic!("signal code not valid"),
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
#[allow(dead_code)]
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
#[allow(dead_code)]
pub const SIG_KERNEL_IGNORE_MASK: SigSet = Signal::into_sigset(Signal::SIGCONT)
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
    /// 判断传入的信号是否被忽略
    ///
    /// ## 参数
    ///
    /// - `sig` 传入的信号
    ///
    /// ## 返回值
    ///
    /// - `true` 被忽略
    /// - `false`未被忽略
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
#[derive(Debug, Clone, Copy)]
pub struct UserSigaction {
    pub handler: *mut core::ffi::c_void,
    pub flags: SigFlags,
    pub restorer: *mut core::ffi::c_void,
    pub mask: SigSet,
}

/**
 * siginfo中，根据signal的来源不同，该info中对应了不同的数据./=
 * 请注意，该info最大占用16字节
 */
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct SigInfo {
    sig_no: i32,
    sig_code: SigCode,
    errno: i32,
    sig_type: SigType,
}

impl SigInfo {
    pub fn sig_code(&self) -> SigCode {
        self.sig_code
    }

    pub fn set_sig_type(&mut self, sig_type: SigType) {
        self.sig_type = sig_type;
    }
    /// @brief 将siginfo结构体拷贝到用户栈
    /// ## 参数
    ///
    /// `to` 用户空间指针
    ///
    /// ## 注意
    ///
    /// 该函数对应Linux中的https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/signal.c#3323
    /// Linux还提供了 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/signal.c#3383 用来实现
    /// kernel_siginfo 保存到 用户的 compact_siginfo 的功能，但是我们系统内还暂时没有对这两种
    /// siginfo做区分，因此暂时不需要第二个函数
    pub fn copy_siginfo_to_user(&self, to: *mut SigInfo) -> Result<i32, SystemError> {
        // 验证目标地址是否为用户空间
        let mut user_buffer = UserBufferWriter::new(to, size_of::<SigInfo>(), true)?;

        let retval: Result<i32, SystemError> = Ok(0);

        user_buffer.copy_one_to_user(self, 0)?;
        return retval;
    }
}

#[derive(Copy, Clone, Debug)]
pub enum SigType {
    Kill(RawPid),
    Alarm(RawPid),
    // 后续完善下列中的具体字段
    // Timer,
    // Rt,
    // SigChild,
    // SigFault,
    // SigPoll,
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
            let mut ret = SigInfo::new(sig, 0, SigCode::User, SigType::Kill(RawPid::from(0)));
            ret.set_sig_type(SigType::Kill(RawPid::new(0)));
            return ret;
        }
    }

    /// @brief 从当前进程的sigpending中取出下一个待处理的signal，并返回给调用者。（调用者应当处理这个信号）
    /// 请注意，进入本函数前，当前进程应当持有current_pcb().sighand.siglock
    pub fn dequeue_signal(&mut self, sig_mask: &SigSet) -> (Signal, Option<SigInfo>) {
        // debug!("dequeue signal");
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
        let filter = |x: &SigInfo| !mask.contains(SigSet::from_bits_truncate(x.sig_no as u64));
        self.queue.q.retain(filter);
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
