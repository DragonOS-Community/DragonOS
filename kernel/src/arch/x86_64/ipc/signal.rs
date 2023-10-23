use core::{ffi::c_void, mem::size_of};

use crate::{
    arch::{
        fpu::FpState,
        interrupt::TrapFrame,
        process::table::{USER_CS, USER_DS},
        sched::sched,
        CurrentIrqArch,
    },
    exception::InterruptArch,
    include::bindings::bindings::USER_MAX_LINEAR_ADDR,
    ipc::{
        signal::set_current_sig_blocked,
        signal_types::{SaHandlerType, SigInfo, Sigaction, SigactionType, SignalArch, MAX_SIG_NUM},
    },
    kdebug, kerror,
    process::ProcessManager,
    syscall::{user_access::UserBufferWriter, Syscall, SystemError},
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
#[derive(Debug, Clone, Copy, Eq)]
#[repr(usize)]
#[allow(non_camel_case_types)]
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
    SIGRTMAX = crate::arch::ipc::signal::_NSIG,
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
        return (*self) as usize <= SIGRTMAX;
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
        return (*self) as usize >= SIGRTMIN;
    }
}

/// siginfo中的si_code的可选值
/// 请注意，当这个值小于0时，表示siginfo来自用户态，否则来自内核态
#[derive(Copy, Debug, Clone)]
#[repr(i32)]
#[allow(non_camel_case_types)]
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

    const SIGHUP = 1<<0;
    const SIGINT= 1<<1;
    const SIGQUIT = 1<<2;
    const SIGILL= 1<<3;
    const SIGTRAP=1<<4;
     /// SIGABRT和SIGIOT共用这个号码
    const SIGABRT_OR_IOT=1<<5;
    const SIGBUS=1<<6;
    const SIGFPE=1<<7;
    const SIGKILL=1<<8;
    const SIGUSR=1<<9;

    const SIGSEGV = 1<<10;
    const SIGUSR2=1<<11;
    const SIGPIPE= 1<<12;
    const SIGALRM=1<<13;
    const SIGTERM=1<<14;
    const SIGSTKFLT=1<<15;
    const SIGCHLD=1<<16;
    const SIGCONT=1<<17;
    const SIGSTOP=1<<18;
    const SIGTSTP=1<<19;

    const SIGTTIN = 1<<20;
    const SIGTTOU = 1<<21;
    const SIGURG=1<<22;
    const SIGXCPU=1<<23;
    const SIGXFSZ=1<<24;
    const SIGVTALRM = 1<<25;
    const SIGPROF = 1<<26;
    const SIGWINCH=1<<27;
    /// SIGIO和SIGPOLL共用这个号码
    const SIGIO_OR_POLL=1<<28;
    const SIGPWR=1<<29;

    const SIGSYS = 1<<30;

    const SIGRTMIN = 1<<31;
    // TODO 写上实时信号
    const SIGRTMAX = 1<<crate::arch::ipc::signal::_NSIG-1;
    }
}

#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
pub struct SigFrame {
    // pub pedding: u64,
    /// 指向restorer的地址的指针。（该变量必须放在sigframe的第一位，因为这样才能在handler返回的时候，跳转到对应的代码，执行sigreturn)
    pub ret_code_ptr: *mut core::ffi::c_void,
    pub handler: *mut c_void,
    pub info: SigInfo,
    pub context: SigContext,
}

#[repr(C, align(16))]
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
    pub reserved_for_x87_state: Option<FpState>,
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
        self.reserved_for_x87_state = ProcessManager::current_pcb().arch_info().fp_state();
        return Ok(0);
    }

    /// @brief 将指定的sigcontext恢复到当前进程的内核栈帧中,并将当前线程结构体的几个参数进行恢复
    ///
    /// @param self 要被恢复的context
    /// @param frame 目标栈帧（也就是把context恢复到这个栈帧中）
    ///
    /// @return bool true -> 成功恢复
    ///              false -> 执行失败
    pub fn restore_sigcontext(&mut self, frame: &mut TrapFrame) -> bool {
        let guard = ProcessManager::current_pcb();
        let mut arch_info = guard.arch_info();
        (*frame) = self.frame.clone();
        kdebug!("restored frame:{:?}", frame);
        // (*current_thread).trap_num = (*context).trap_num;
        *arch_info.cr2_mut() = self.cr2 as usize;
        // (*current_thread).err_code = (*context).err_code;
        // 如果当前进程有fpstate，则将其恢复到pcb的fp_state中
        *arch_info.fp_state_mut() = self.reserved_for_x87_state.clone();
        arch_info.restore_fp_state();
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

pub struct X86_64SignalArch;

impl SignalArch for X86_64SignalArch {
    #[no_mangle]
    unsafe fn do_signal(frame: &mut TrapFrame) {
        // 检查sigpending是否为0
        if ProcessManager::current_pcb()
            .sig_info()
            .sig_pending()
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
        let pcb = ProcessManager::current_pcb();
        let sig_guard = pcb.sig_struct();
        let mut sig_number: Signal;
        let mut info: Option<SigInfo>;
        let sigaction: Option<&mut Sigaction>;
        let mut tmp_ka: Sigaction;
        let reader = pcb.sig_info();
        let sig_block: SigSet = reader.sig_block().clone();
        drop(reader);
        loop {
            (sig_number, info) = pcb.sig_info_mut().dequeue_signal(&sig_block);
            // 如果信号非法，则直接返回
            if sig_number == Signal::INVALID {
                return;
            }

            tmp_ka = sig_guard.handlers[sig_number as usize - 1];
            match tmp_ka.action() {
                SigactionType::SaHandler(action_type) => match action_type {
                    SaHandlerType::SigError => {
                        kerror!("Trying to handle a Sigerror on Process:{:?}", pcb.pid());
                        return;
                    }
                    SaHandlerType::SigDefault => {
                        tmp_ka = Sigaction::default();
                        sigaction = Some(&mut tmp_ka);
                        break;
                    }
                    SaHandlerType::SigIgnore => continue,
                    SaHandlerType::SigCustomized(_) => {
                        sigaction = Some(&mut tmp_ka);
                        break;
                    }
                },
                SigactionType::SaSigaction(_) => todo!(),
            }
            // 如果当前动作是忽略这个信号，就继续循环。
        }
        // 所有的信号都处理完了
        assert!(sigaction.is_some());
        let reader = pcb.sig_info();
        let oldset = reader.sig_block().clone();
        //避免死锁
        drop(reader);
        drop(sig_guard);
        let res: Result<i32, SystemError> = handle_signal(
            sig_number,
            sigaction.unwrap(),
            &info.unwrap(),
            &oldset,
            frame,
        );
        if res.is_err() {
            kerror!(
                "Error occurred when handling signal: {}, pid={:?}, errcode={:?}",
                sig_number as i32,
                ProcessManager::current_pcb().pid(),
                res.unwrap_err()
            );
        }
    }

    #[no_mangle]
    fn sys_rt_sigreturn(trap_frame: &mut TrapFrame) -> u64 {
        kdebug!("trap_frame in sigreturn:{:?}", trap_frame);
        let frame = (trap_frame.rsp as usize) as *mut SigFrame;

        // 如果当前的rsp不来自用户态，则认为产生了错误（或被SROP攻击）
        if UserBufferWriter::new(frame, size_of::<SigFrame>(), true).is_err() {
            kerror!("rsp doesn't from user level");
            let _r = Syscall::kill(ProcessManager::current_pcb().pid(), Signal::SIGSEGV as i32)
                .map_err(|e| e.to_posix_errno());
        }
        let mut sigmask: SigSet = unsafe { (*frame).context.oldmask };
        set_current_sig_blocked(&mut sigmask);
        // 从用户栈恢复sigcontext
        if !unsafe { &mut (*frame).context }.restore_sigcontext(trap_frame) {
            kerror!("unable to restore sigcontext");
            let _r = Syscall::kill(ProcessManager::current_pcb().pid(), Signal::SIGSEGV as i32)
                .map_err(|e| e.to_posix_errno());
            // 如果这里返回 err 值的话会丢失上一个系统调用的返回值
        }
        // 由于系统调用的返回值会被系统调用模块被存放在rax寄存器，因此，为了还原原来的那个系统调用的返回值，我们需要在这里返回恢复后的rax的值
        return trap_frame.rax;
    }
}

/// @brief 真正发送signal，执行自定义的处理函数
///
/// @param sig 信号number
/// @param sigaction 信号响应动作
/// @param info 信号信息
/// @param oldset
/// @param regs 之前的系统调用将要返回的时候，要弹出的栈帧的拷贝
///
/// @return Result<0,SystemError> 若Error, 则返回错误码,否则返回Ok(0)
fn handle_signal(
    sig: Signal,
    sigaction: &mut Sigaction,
    info: &SigInfo,
    oldset: &SigSet,
    frame: &mut TrapFrame,
) -> Result<i32, SystemError> {
    // TODO 这里要补充一段逻辑，好像是为了保证引入线程之后的地址空间不会出问题。详见https://opengrok.ringotek.cn/xref/linux-6.1.9/arch/mips/kernel/signal.c#830

    // 设置栈帧
    return setup_frame(sig, sigaction, info, oldset, frame);
}

/// @brief 在用户栈上开辟一块空间，并且把内核栈的栈帧以及需要在用户态执行的代码给保存进去。
///
/// @param regs 进入信号处理流程前，Restore all要弹出的内核栈栈帧
fn setup_frame(
    sig: Signal,
    sigaction: &mut Sigaction,
    info: &SigInfo,
    oldset: &SigSet,
    trap_frame: &mut TrapFrame,
) -> Result<i32, SystemError> {
    let ret_code_ptr: *mut c_void;
    let temp_handler: *mut c_void;
    match sigaction.action() {
        SigactionType::SaHandler(handler_type) => match handler_type {
            SaHandlerType::SigDefault => {
                sig_default_handler(sig);
                return Ok(0);
            }
            SaHandlerType::SigCustomized(handler) => {
                // 如果handler位于内核空间
                if handler >= USER_MAX_LINEAR_ADDR {
                    // 如果当前是SIGSEGV,则采用默认函数处理
                    if sig == Signal::SIGSEGV {
                        sig_default_handler(sig);
                        return Ok(0);
                    } else {
                        kerror!("attempting  to execute a signal handler from kernel");
                        sig_default_handler(sig);
                        return Err(SystemError::EINVAL);
                    }
                } else {
                    // 为了与Linux的兼容性，64位程序必须由用户自行指定restorer
                    if sigaction.flags().contains(SigFlags::SA_RESTORER) {
                        ret_code_ptr = sigaction.restorer().unwrap() as usize as *mut c_void;
                    } else {
                        kerror!(
                            "pid-{:?} forgot to set SA_FLAG_RESTORER for signal {:?}",
                            ProcessManager::current_pcb().pid(),
                            sig as i32
                        );
                        let r = Syscall::kill(
                            ProcessManager::current_pcb().pid(),
                            Signal::SIGSEGV as i32,
                        );
                        if r.is_err() {
                            kerror!("In setup_sigcontext: generate SIGSEGV signal failed");
                        }
                        return Err(SystemError::EINVAL);
                    }
                    if sigaction.restorer().is_none() {
                        kerror!(
                            "restorer in process:{:?} is not defined",
                            ProcessManager::current_pcb().pid()
                        );
                        return Err(SystemError::EINVAL);
                    }
                    temp_handler = handler as usize as *mut c_void;
                }
            }
            SaHandlerType::SigIgnore => {
                return Ok(0);
            }
            _ => {
                return Err(SystemError::EINVAL);
            }
        },
        SigactionType::SaSigaction(_) => {
            //TODO 这里应该是可以恢复栈的，等后续来做
            kerror!("trying to recover from sigaction type instead of handler");
            return Err(SystemError::EINVAL);
        }
    }
    let frame: *mut SigFrame = get_stack(&trap_frame, size_of::<SigFrame>());
    // kdebug!("frame=0x{:016x}", frame as usize);
    // 要求这个frame的地址位于用户空间，因此进行校验
    let r = UserBufferWriter::new(frame, size_of::<SigFrame>(), true);
    if r.is_err() {
        // 如果地址区域位于内核空间，则直接报错
        // todo: 生成一个sigsegv
        let r = Syscall::kill(ProcessManager::current_pcb().pid(), Signal::SIGSEGV as i32);
        if r.is_err() {
            kerror!("In setup frame: generate SIGSEGV signal failed");
        }
        kerror!("In setup frame: access check failed");
        return Err(SystemError::EPERM);
    }

    // 将siginfo拷贝到用户栈
    info.copy_siginfo_to_user(unsafe { &mut ((*frame).info) as *mut SigInfo })
        .map_err(|e| -> SystemError {
            let r = Syscall::kill(ProcessManager::current_pcb().pid(), Signal::SIGSEGV as i32);
            if r.is_err() {
                kerror!("In copy_siginfo_to_user: generate SIGSEGV signal failed");
            }
            return e;
        })?;

    // todo: 拷贝处理程序备用栈的地址、大小、ss_flags

    unsafe {
        (*frame)
            .context
            .setup_sigcontext(oldset, &trap_frame)
            .map_err(|e: SystemError| -> SystemError {
                let r = Syscall::kill(ProcessManager::current_pcb().pid(), Signal::SIGSEGV as i32);
                if r.is_err() {
                    kerror!("In setup_sigcontext: generate SIGSEGV signal failed");
                }
                return e;
            })?
    };

    // 保存完毕后，清空fp_state，以免下次save的时候，出现SIMD exception
    ProcessManager::current_pcb().arch_info().clear_fp_state();
    unsafe {
        // 在开头检验过sigaction.restorer是否为空了，实际上libc会保证 restorer始终不为空
        (*frame).ret_code_ptr = ret_code_ptr;
    }

    unsafe { (*frame).handler = temp_handler };
    // 传入信号处理函数的第一个参数
    trap_frame.rdi = sig as u64;
    trap_frame.rsi = unsafe { &(*frame).info as *const SigInfo as u64 };
    trap_frame.rsp = frame as u64;
    trap_frame.rip = unsafe { (*frame).handler as u64 };
    kdebug!("trap_frame{:?}", trap_frame);
    // 设置cs和ds寄存器
    trap_frame.cs = (USER_CS.bits() | 0x3) as u64;
    trap_frame.ds = (USER_DS.bits() | 0x3) as u64;

    // 禁用中断
    // trap_frame.rflags &= !(0x200);

    return Ok(0);
}

#[inline(always)]
fn get_stack(frame: &TrapFrame, size: usize) -> *mut SigFrame {
    // TODO:在 linux 中会根据 Sigaction 中的一个flag 的值来确定是否使用pcb中的 signal 处理程序备用堆栈，现在的
    // pcb中也没有这个备用堆栈

    // 默认使用 用户栈的栈顶指针-128字节的红区-sigframe的大小 并且16字节对齐
    let mut rsp: usize = (frame.rsp as usize) - 128 - size;
    // 按照要求进行对齐，别问为什么减8，不减8就是错的，可以看
    // https://sourcegraph.com/github.com/torvalds/linux@dd72f9c7e512da377074d47d990564959b772643/-/blob/arch/x86/kernel/signal.c?L124
    // 我猜测是跟x86汇编的某些弹栈行为有关系，它可能会出于某种原因递增 rsp
    rsp &= (!(STACK_ALIGN - 1)) as usize - 8;
    // rsp &= (!(STACK_ALIGN - 1)) as usize;
    return rsp as *mut SigFrame;
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

/// 分发默认处理函数，只要调用了这个默认处理函数，就不会进入到后面伪造栈帧的环节，会直接在
/// 执行完处理函数之后返回，也不会调用 sig_return 恢复栈帧
#[no_mangle]
fn sig_default_handler(sig: Signal) {
    match sig {
        Signal::INVALID => panic!(),
        Signal::SIGHUP => sig_terminate(sig),
        Signal::SIGINT => sig_terminate(sig),
        Signal::SIGQUIT => sig_terminate_dump(sig),
        Signal::SIGILL => sig_terminate_dump(sig),
        Signal::SIGTRAP => sig_terminate_dump(sig),
        Signal::SIGABRT_OR_IOT => sig_terminate_dump(sig),
        Signal::SIGBUS => sig_terminate_dump(sig),
        Signal::SIGFPE => sig_terminate_dump(sig),
        Signal::SIGKILL => sig_terminate(sig),
        Signal::SIGUSR1 => sig_terminate(sig),
        Signal::SIGSEGV => sig_terminate_dump(sig),
        Signal::SIGUSR2 => sig_terminate(sig),
        Signal::SIGPIPE => sig_terminate(sig),
        Signal::SIGALRM => sig_terminate(sig),
        Signal::SIGTERM => sig_terminate(sig),
        Signal::SIGSTKFLT => sig_terminate(sig),
        Signal::SIGCHLD => sig_ignore(sig),
        Signal::SIGCONT => sig_continue(sig),
        Signal::SIGSTOP => sig_stop(sig),
        Signal::SIGTSTP => sig_stop(sig),
        Signal::SIGTTIN => sig_stop(sig),
        Signal::SIGTTOU => sig_stop(sig),
        Signal::SIGURG => sig_ignore(sig),
        Signal::SIGXCPU => sig_terminate_dump(sig),
        Signal::SIGXFSZ => sig_terminate_dump(sig),
        Signal::SIGVTALRM => sig_terminate(sig),
        Signal::SIGPROF => sig_terminate(sig),
        Signal::SIGWINCH => sig_ignore(sig),
        Signal::SIGIO_OR_POLL => sig_terminate(sig),
        Signal::SIGPWR => sig_terminate(sig),
        Signal::SIGSYS => sig_terminate(sig),
        Signal::SIGRTMIN => sig_terminate(sig),
        Signal::SIGRTMAX => sig_terminate(sig),
    }
}
