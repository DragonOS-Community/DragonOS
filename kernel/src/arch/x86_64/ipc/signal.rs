use core::{ffi::c_void, mem::size_of, ops::BitOr};

use alloc::sync::Arc;

use crate::{
    arch::{
        fpu::FpState,
        interrupt::TrapFrame,
        process::table::{USER_CS, USER_DS},
        CurrentIrqArch,
    },
    exception::InterruptArch,
    include::bindings::bindings::USER_MAX_LINEAR_ADDR,
    ipc::{
        signal::{get_signal_to_deliver, set_current_sig_blocked},
        signal_types::{SigInfo, SigType, Sigaction, SigactionType, MAX_SIG_NUM},
    },
    kdebug, kerror,
    process::{Pid, ProcessManager},
    syscall::{user_access::UserBufferWriter, SystemError},
};

/// 最大支持的信号数量
pub const _NSIG: usize = 64;
/// 实时信号的最小值
pub const SIGRTMIN: usize = 32;
/// 实时信号的最大值
pub const SIGRTMAX: usize = crate::arch::ipc::signal::_NSIG;
/// 信号处理的栈的栈指针的最小对齐数量
pub const STACK_ALIGN: u64 = 16;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
#[repr(usize)]
pub enum SignalNumber {
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
    SIGRTMAX = crate::arch::ipc::signal::_NSIG,
}

/// 为SignalNumber实现判断相等的trait
impl PartialEq for SignalNumber {
    fn eq(&self, other: &SignalNumber) -> bool {
        *self as usize == *other as usize
    }
}

impl From<usize> for SignalNumber {
    fn from(value: usize) -> Self {
        if value <= MAX_SIG_NUM {
            let ret: SignalNumber = unsafe { core::mem::transmute(value) };
            return ret;
        } else {
            kerror!("Try to convert an invalid number to SignalNumber");
            return SignalNumber::INVALID;
        }
    }
}

impl Into<usize> for SignalNumber {
    fn into(self) -> usize {
        self as usize
    }
}

impl From<i32> for SignalNumber {
    fn from(value: i32) -> Self {
        if value < 0 {
            kerror!("Try to convert an invalid number to SignalNumber");
            return SignalNumber::INVALID;
        } else {
            return Self::from(value as usize);
        }
    }
}

impl Into<SigSet> for SignalNumber {
    fn into(self) -> SigSet {
        self.into_sigset()
    }
}
impl SignalNumber {
    /// 判断一个数字是否为可用的信号
    #[inline]
    pub fn is_valid(&self) -> bool {
        return (*self) as usize <= SIGRTMAX;
    }

    /// const convertor between `SignalNumber` and `SigSet`
    pub const fn into_sigset(self) -> SigSet {
        SigSet::from_bits_truncate((self as usize - 1) as u64)
    }
}

/// siginfo中的si_code的可选值
/// 请注意，当这个值小于0时，表示siginfo来自用户态，否则来自内核态
#[derive(Copy, Debug, Clone)]
#[repr(i32)]
pub enum SigCode {
    /// sent by kill, sigsend, raise
    SI_USER = 0,
    /// sent by kernel from somewhere
    SI_KERNEL = 0x80,
    /// 通过sigqueue发送
    SI_QUEUE = -1,
    /// 定时器过期时发送
    SI_TIMER = -2,
    /// 当实时消息队列的状态发生改变时发送
    SI_MESGQ = -3,
    /// 当异步IO完成时发送
    SI_ASYNCIO = -4,
    /// sent by queued SIGIO
    SI_SIGIO = -5,
}

impl SigCode {
    /// 为SigCode这个枚举类型实现从i32转换到枚举类型的转换函数
    #[allow(dead_code)]
    pub fn from_i32(x: i32) -> SigCode {
        match x {
            0 => Self::SI_USER,
            0x80 => Self::SI_KERNEL,
            -1 => Self::SI_QUEUE,
            -2 => Self::SI_TIMER,
            -3 => Self::SI_MESGQ,
            -4 => Self::SI_ASYNCIO,
            -5 => Self::SI_SIGIO,
            _ => panic!("signal code not valid"),
        }
    }
}

bitflags! {
    #[derive(Default)]
    pub struct SigFlags:u32{
        const SA_FLAG_DFL= 1 << 0; // 当前sigaction表示系统默认的动作
        const SA_FLAG_IGN = 1 << 1; // 当前sigaction表示忽略信号的动作
        const SA_FLAG_RESTORER = 1 << 2; // 当前sigaction具有用户指定的restorer
        const SA_FLAG_IMMUTABLE = 1 << 3; // 当前sigaction不可被更改
        const SA_FLAG_ALL = Self::SA_FLAG_DFL.bits()|Self::SA_FLAG_DFL.bits()|Self::SA_FLAG_IGN.bits()|Self::SA_FLAG_IMMUTABLE.bits()|Self::SA_FLAG_RESTORER.bits();
    }

    /// 请注意，sigset 这个bitmap, 第0位表示sig=1的信号。也就是说，SignalNumber-1才是sigset_t中对应的位
    #[derive(Default)]
    pub struct SigSet:u64{
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SigFrame {
    /// 指向restorer的地址的指针。（该变量必须放在sigframe的第一位，因为这样才能在handler返回的时候，跳转到对应的代码，执行sigreturn)
    pub ret_code_ptr: *mut core::ffi::c_void,
    /// signum
    pub arg0: u64,
    /// siginfo pointer
    pub arg1: usize,
    /// sigcontext pointer
    pub arg2: usize,

    pub handler: *mut c_void,
    pub info: SigInfo,
    pub context: SigContext,
}

#[derive(Debug, Clone, Copy)]
pub struct SigContext {
    /// sigcontext的标志位
    pub sc_flags: u64,
    pub sc_stack: SigStack, // 信号处理程序备用栈信息
    pub frame: TrapFrame,   // 暂存的系统调用/中断返回时，原本要弹出的内核栈帧
    // pub trap_num: u64,    // 用来保存线程结构体中的trap_num字段
    pub oldmask: SigSet, // 暂存的执行信号处理函数之前的，被设置block的信号
    pub cr2: u64,        // 用来保存线程结构体中的cr2字段
    // pub err_code: u64,    // 用来保存线程结构体中的err_code字段
    // todo: 支持x87浮点处理器后，在这里增加浮点处理器的状态结构体指针
    pub reserved_for_x87_state: u64,
    pub reserved: [u64; 8],
}

impl SigContext {
    /// @brief 设置目标的sigcontext
    ///
    /// @param context 要被设置的目标sigcontext
    /// @param mask 要被暂存的信号mask标志位
    /// @param regs 进入信号处理流程前，Restore all要弹出的内核栈栈帧
    pub fn setup_sigcontext(
        &mut self,
        mask: &SigSet,
        frame: &TrapFrame,
    ) -> Result<i32, SystemError> {
        //TODO 引入线程后补上
        // let current_thread = ProcessManager::current_pcb().thread;

        self.oldmask = *mask;
        self.frame = frame.clone();
        // context.trap_num = unsafe { (*current_thread).trap_num };
        // context.err_code = unsafe { (*current_thread).err_code };
        // context.cr2 = unsafe { (*current_thread).cr2 };
        return Ok(0);
    }

    /// @brief 将指定的sigcontext恢复到当前进程的内核栈帧中,并将当前线程结构体的几个参数进行恢复
    ///
    /// @param context 要被恢复的context
    /// @param regs 目标栈帧（也就是把context恢复到这个栈帧中）
    ///
    /// @return bool true -> 成功恢复
    ///              false -> 执行失败
    pub fn restore_sigcontext(&self, frame: &mut TrapFrame) -> bool {
        let guard = ProcessManager::current_pcb();
        let mut arch_info = guard.arch_info();
        *frame = self.frame;

        // (*current_thread).trap_num = (*context).trap_num;
        *arch_info.cr2_mut() = self.cr2 as usize;
        // (*current_thread).err_code = (*context).err_code;
        // 如果当前进程有fpstate，则将其恢复到pcb的fp_state中
        ProcessManager::current_pcb().arch_info().restore_fp_state();
        return true;
    }
}
/// @brief 信号处理备用栈的信息
#[derive(Debug, Clone, Copy)]
pub struct SigStack {
    pub sp: *mut c_void,
    pub flags: u32,
    pub size: u32,
    pub fpstate: FpState,
}

#[no_mangle]
pub unsafe extern "C" fn do_signal(frame: &mut TrapFrame) {
    // 检查sigpending是否为0
    if ProcessManager::current_pcb()
        .sig_info()
        .sig_pedding()
        .signal()
        .bits()
        == 0
        || !frame.from_user()
    {
        // 若没有正在等待处理的信号，或者将要返回到的是内核态，则启用中断，然后返回
        CurrentIrqArch::interrupt_enable();
        return;
    }

    // 做完上面的检查后，开中断
    CurrentIrqArch::interrupt_enable();

    let oldset = ProcessManager::current_pcb().sig_info().sig_block();
    loop {
        let (sig_number, info, ka) = get_signal_to_deliver(&frame.clone());
        // 所有的信号都处理完了
        if sig_number == SignalNumber::INVALID {
            return;
        }
        kdebug!(
            "To handle signal [{}] for pid:{:?}",
            sig_number as i32,
            ProcessManager::current_pcb().pid(),
        );
        assert!(ka.is_some());
        let res = handle_signal(sig_number, ka.unwrap(), &info.unwrap(), &oldset, frame);
        if res.is_err() {
            kerror!(
                "Error occurred when handling signal: {}, pid={:?}, errcode={:?}",
                sig_number as i32,
                ProcessManager::current_pcb().pid(),
                res.unwrap_err()
            );
        }
    }
}

/// @brief 真正发送signal，执行自定义的处理函数
///
/// @param sig 信号number
/// @param ka 信号响应动作
/// @param info 信号信息
/// @param oldset
/// @param regs 之前的系统调用将要返回的时候，要弹出的栈帧的拷贝
///
/// @return Result<0,SystemError> 若Error, 则返回错误码,否则返回Ok(0)
fn handle_signal(
    sig: SignalNumber,
    ka: &mut Sigaction,
    info: &SigInfo,
    oldset: &SigSet,
    frame: &mut TrapFrame,
) -> Result<i32, SystemError> {
    // 设置栈帧
    let retval = setup_frame(sig, ka, info, oldset, frame);
    if retval.is_err() {
        return retval;
    }
    return Ok(0);
}

/// @brief 在用户栈上开辟一块空间，并且把内核栈的栈帧以及需要在用户态执行的代码给保存进去。
///
/// @param regs 进入信号处理流程前，Restore all要弹出的内核栈栈帧
fn setup_frame(
    sig: SignalNumber,
    ka: &mut Sigaction,
    info: &SigInfo,
    oldset: &SigSet,
    trap_frame: &mut TrapFrame,
) -> Result<i32, SystemError> {
    let mut err = 0;
    let frame: *mut SigFrame = get_stack(ka, &trap_frame, size_of::<SigFrame>());
    // kdebug!("frame=0x{:016x}", frame as usize);
    // 要求这个frame的地址位于用户空间，因此进行校验
    let r = UserBufferWriter::new(frame, size_of::<SigFrame>(), true);
    if r.is_err() {
        // 如果地址区域位于内核空间，则直接报错
        // todo: 生成一个sigsegv
        kerror!("In setup frame: access check failed");
        return Err(SystemError::EPERM);
    }
    if ka.restorer().is_none() {
        kerror!(
            "restorer in process:{:?} is not defined",
            ProcessManager::current_pcb().pid()
        );
        return Err(SystemError::EINVAL);
    }

    match ka.action() {
        SigactionType::SaHandler(handler) => {
            if handler.is_sig_default() {
                kerror!("In setup frame: handler is None");
                return Err(SystemError::EINVAL);
            }
            unsafe {
                (*frame).arg0 = sig as u64;
                (*frame).arg1 = &((*frame).info) as *const SigInfo as usize;
                (*frame).arg2 = &((*frame).context) as *const SigContext as usize;
                (*frame).handler = Into::<usize>::into(handler) as usize as *mut c_void;
            }
        }
        SigactionType::SaSigaction(_) => {
            //TODO 这里应该是可以恢复的栈的，等后续来做
            kerror!("trying to recover from sigaction type instead of handler");
            return Err(SystemError::EINVAL);
        }
    }

    // 将当前进程的fp_state拷贝到用户栈
    ProcessManager::current_pcb().arch_info().save_fp_state();
    // 保存完毕后，清空fp_state，以免下次save的时候，出现SIMD exception
    ProcessManager::current_pcb().arch_info().clear_fp_state();

    // 将siginfo拷贝到用户栈
    err |= info
        .copy_siginfo_to_user(&mut unsafe { (*frame).info })
        .unwrap_or(1);

    // todo: 拷贝处理程序备用栈的地址、大小、ss_flags

    err |= unsafe {
        (*frame)
            .context
            .setup_sigcontext(oldset, &trap_frame)
            .unwrap_or(1)
    };

    // 为了与Linux的兼容性，64位程序必须由用户自行指定restorer
    if ka.flags().contains(SigFlags::SA_FLAG_RESTORER) {
        unsafe {
            // 在开头检验过ka.restorer是否为空了
            (*frame).ret_code_ptr = ka.restorer().unwrap() as usize as *mut c_void;
        }
    } else {
        kerror!(
            "pid-{:?} forgot to set SA_FLAG_RESTORER for signal {:?}",
            ProcessManager::current_pcb().pid(),
            sig as i32
        );
        err = 1;
    }
    if err != 0 {
        // todo: 在这里生成一个sigsegv,然后core dump
        //临时解决方案：退出当前进程
        ProcessManager::exit(1);
    }
    // 传入信号处理函数的第一个参数
    trap_frame.rdi = sig as u64;
    trap_frame.rsi = unsafe { &(*frame).info as *const SigInfo as u64 };
    trap_frame.rsp = frame as u64;
    trap_frame.rip = ka.restorer().unwrap();

    // todo: 传入新版的sa_sigaction的处理函数的第三个参数

    // 如果handler位于内核空间
    if trap_frame.rip >= USER_MAX_LINEAR_ADDR {
        // 如果当前是SIGSEGV,则采用默认函数处理
        if sig == SignalNumber::SIGSEGV {
            ka.flags_mut().insert(SigFlags::SA_FLAG_DFL);
        }

        // 将rip设置为0
        trap_frame.rip = 0;
    }
    // 设置cs和ds寄存器
    trap_frame.cs = (USER_CS.bits() | 0x3) as u64;
    trap_frame.ds = (USER_DS.bits() | 0x3) as u64;

    return if err == 0 {
        Ok(0)
    } else {
        Err(SystemError::EPERM)
    };
}

#[inline(always)]
fn get_stack(_ka: &Sigaction, frame: &TrapFrame, size: usize) -> *mut SigFrame {
    // 默认使用 用户栈的栈顶指针-128字节的红区-sigframe的大小，在 linux 中会根据 Sigaction 中的一个flag 的值来确定是否使用
    // pcb中的 signal 处理程序备用堆栈 信号处理程序备用堆栈的地址
    let mut rsp: usize = (frame.rsp as usize) - 128 - size;
    // 按照要求进行对齐
    rsp &= (-(STACK_ALIGN as i64)) as usize;
    return rsp as *mut SigFrame;
}

pub fn sys_rt_sigreturn(trap_frame: &mut TrapFrame) -> u64 {
    let frame = trap_frame.rsp as usize as *mut SigFrame;

    // 如果当前的rsp不来自用户态，则认为产生了错误（或被SROP攻击）
    if UserBufferWriter::new(frame, size_of::<SigFrame>(), true).is_err() {
        // todo：这里改为生成一个sigsegv
        // 退出进程
        ProcessManager::exit(SignalNumber::SIGSEGV as usize);
    }

    let mut sigmask: SigSet = unsafe { (*frame).context.oldmask };
    set_current_sig_blocked(&mut sigmask);

    // 从用户栈恢复sigcontext
    if unsafe { &mut (*frame).context }.restore_sigcontext(trap_frame) == false {
        // todo：这里改为生成一个sigsegv
        // 退出进程
        ProcessManager::exit(SignalNumber::SIGSEGV as usize);
    }

    // 由于系统调用的返回值会被系统调用模块被存放在rax寄存器，因此，为了还原原来的那个系统调用的返回值，我们需要在这里返回恢复后的rax的值
    return trap_frame.rax;
}
