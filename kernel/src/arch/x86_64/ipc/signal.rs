use core::sync::atomic::{compiler_fence, Ordering};
use core::{ffi::c_void, intrinsics::unlikely, mem::size_of};

use defer::defer;
use log::error;
use system_error::SystemError;

pub use crate::ipc::generic_signal::AtomicGenericSignal as AtomicSignal;
pub use crate::ipc::generic_signal::GenericSigChildCode as SigChildCode;
pub use crate::ipc::generic_signal::GenericSigSet as SigSet;
pub use crate::ipc::generic_signal::GenericSignal as Signal;

use crate::{
    arch::{
        fpu::FpState,
        interrupt::TrapFrame,
        process::table::{USER_CS, USER_DS},
        syscall::nr::SYS_RESTART_SYSCALL,
        CurrentIrqArch, MMArch,
    },
    exception::InterruptArch,
    ipc::{
        signal::{restore_saved_sigmask, set_current_blocked},
        signal_types::{
            PosixSigInfo, SaHandlerType, SigInfo, Sigaction, SigactionType, SignalArch, SignalFlags,
        },
    },
    mm::MemoryManagementArch,
    process::ProcessManager,
    syscall::user_access::UserBufferWriter,
};

/// 信号处理的栈的栈指针的最小对齐数量
pub const STACK_ALIGN: u64 = 16;
/// 信号最大值
pub const MAX_SIG_NUM: usize = 64;

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

#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
pub struct SigFrame {
    // pub pedding: u64,
    /// 指向restorer的地址的指针。（该变量必须放在sigframe的第一位，因为这样才能在handler返回的时候，跳转到对应的代码，执行sigreturn)
    pub ret_code_ptr: *mut core::ffi::c_void,
    pub handler: *mut c_void,
    pub info: PosixSigInfo,
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
    /// 设置sigcontext
    ///
    /// ## 参数
    ///
    /// - `mask` 要被暂存的信号mask标志位
    /// - `regs` 进入信号处理流程前，Restore all要弹出的内核栈栈帧
    ///
    /// ## 返回值
    ///
    /// - `Ok(0)`
    /// - `Err(Systemerror)` (暂时不会返回错误)
    pub fn setup_sigcontext(
        &mut self,
        mask: &SigSet,
        frame: &TrapFrame,
    ) -> Result<i32, SystemError> {
        //TODO 引入线程后补上
        // let current_thread = ProcessManager::current_pcb().thread;
        let pcb = ProcessManager::current_pcb();
        let mut archinfo_guard = pcb.arch_info_irqsave();
        self.oldmask = *mask;
        self.frame = *frame;
        // context.trap_num = unsafe { (*current_thread).trap_num };
        // context.err_code = unsafe { (*current_thread).err_code };
        // context.cr2 = unsafe { (*current_thread).cr2 };
        self.reserved_for_x87_state = *archinfo_guard.fp_state();

        // 保存完毕后，清空fp_state，以免下次save的时候，出现SIMD exception
        archinfo_guard.clear_fp_state();
        return Ok(0);
    }

    /// 指定的sigcontext恢复到当前进程的内核栈帧中,并将当前线程结构体的几个参数进行恢复
    ///
    /// ## 参数
    /// - `frame` 目标栈帧（也就是把context恢复到这个栈帧中）
    ///
    /// ##返回值
    /// - `true` -> 成功恢复
    /// - `false` -> 执行失败
    pub fn restore_sigcontext(&mut self, frame: &mut TrapFrame) -> bool {
        let guard = ProcessManager::current_pcb();
        let mut arch_info = guard.arch_info_irqsave();
        (*frame) = self.frame;
        // (*current_thread).trap_num = (*context).trap_num;
        *arch_info.cr2_mut() = self.cr2 as usize;
        // (*current_thread).err_code = (*context).err_code;
        // 如果当前进程有fpstate，则将其恢复到pcb的fp_state中
        *arch_info.fp_state_mut() = self.reserved_for_x87_state;
        arch_info.restore_fp_state();
        return true;
    }
}
/// @brief 信号处理备用栈的信息
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct SigStack {
    pub sp: *mut c_void,
    pub flags: u32,
    pub size: u32,
    pub fpstate: FpState,
}

unsafe fn do_signal(frame: &mut TrapFrame, got_signal: &mut bool) {
    let pcb = ProcessManager::current_pcb();

    let siginfo = pcb.try_siginfo_irqsave(5);

    if unlikely(siginfo.is_none()) {
        return;
    }

    let siginfo_read_guard = siginfo.unwrap();

    // 检查sigpending是否为0
    if siginfo_read_guard.sig_pending().signal().bits() == 0 || !frame.is_from_user() {
        // 若没有正在等待处理的信号，或者将要返回到的是内核态，则返回
        return;
    }

    let mut sig_number: Signal;
    let mut info: Option<SigInfo>;
    let mut sigaction: Option<Sigaction>;
    let sig_block: SigSet = *siginfo_read_guard.sig_blocked();
    drop(siginfo_read_guard);

    // x86_64 上不再需要 sig_struct 自旋锁
    let siginfo_mut = pcb.try_siginfo_mut(5);
    if unlikely(siginfo_mut.is_none()) {
        return;
    }

    let mut siginfo_mut_guard = siginfo_mut.unwrap();
    loop {
        (sig_number, info) = siginfo_mut_guard.dequeue_signal(&sig_block, &pcb);

        // 如果信号非法，则直接返回
        if sig_number == Signal::INVALID {
            return;
        }
        let sa = pcb.sighand().handler(sig_number).unwrap();

        match sa.action() {
            SigactionType::SaHandler(action_type) => match action_type {
                SaHandlerType::Error => {
                    error!("Trying to handle a Sigerror on Process:{:?}", pcb.raw_pid());
                    return;
                }
                SaHandlerType::Default => {
                    sigaction = Some(sa);
                }
                SaHandlerType::Ignore => continue,
                SaHandlerType::Customized(_) => {
                    sigaction = Some(sa);
                }
            },
            SigactionType::SaSigaction(_) => todo!(),
        }

        /*
         * Global init gets no signals it doesn't want.
         * Container-init gets no signals it doesn't want from same
         * container.
         *
         * Note that if global/container-init sees a sig_kernel_only()
         * signal here, the signal must have been generated internally
         * or must have come from an ancestor namespace. In either
         * case, the signal cannot be dropped.
         */
        // todo: https://code.dragonos.org.cn/xref/linux-6.6.21/include/linux/signal.h?fi=sig_kernel_only#444
        if ProcessManager::current_pcb()
            .sighand()
            .flags_contains(SignalFlags::UNKILLABLE)
            && !sig_number.kernel_only()
        {
            continue;
        }

        if sigaction.is_some() {
            break;
        }
    }

    let oldset = *siginfo_mut_guard.sig_blocked();
    //避免死锁
    drop(siginfo_mut_guard);
    // no sig_struct guard to drop
    drop(pcb);
    // 做完上面的检查后，开中断
    CurrentIrqArch::interrupt_enable();

    if sigaction.is_none() {
        return;
    }
    *got_signal = true;

    let mut sigaction = sigaction.unwrap();

    // 注意！由于handle_signal里面可能会退出进程，
    // 因此这里需要检查清楚：上面所有的锁、arc指针都被释放了。否则会产生资源泄露的问题！
    let res: Result<i32, SystemError> =
        handle_signal(sig_number, &mut sigaction, &info.unwrap(), &oldset, frame);
    compiler_fence(Ordering::SeqCst);
    if res.is_err() {
        error!(
            "Error occurred when handling signal: {}, pid={:?}, errcode={:?}",
            sig_number as i32,
            ProcessManager::current_pcb().raw_pid(),
            res.as_ref().unwrap_err()
        );
    }
}

fn try_restart_syscall(frame: &mut TrapFrame) {
    defer!({
        // 如果没有信号需要传递，我们只需恢复保存的信号掩码
        restore_saved_sigmask();
    });

    if unsafe { frame.syscall_nr() }.is_none() {
        return;
    }

    let syscall_err = unsafe { frame.syscall_error() };
    if syscall_err.is_none() {
        return;
    }
    let syscall_err = syscall_err.unwrap();

    let mut restart = false;
    match syscall_err {
        SystemError::ERESTARTSYS | SystemError::ERESTARTNOHAND | SystemError::ERESTARTNOINTR => {
            frame.rax = frame.errcode;
            frame.rip -= 2;
            restart = true;
        }
        SystemError::ERESTART_RESTARTBLOCK => {
            frame.rax = SYS_RESTART_SYSCALL as u64;
            frame.rip -= 2;
            restart = true;
        }
        _ => {}
    }
    log::debug!("try restart syscall: {:?}", restart);
}

pub struct X86_64SignalArch;

impl SignalArch for X86_64SignalArch {
    /// 处理信号，并尝试重启系统调用
    ///
    /// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/signal.c#865
    unsafe fn do_signal_or_restart(frame: &mut TrapFrame) {
        let mut got_signal = false;
        do_signal(frame, &mut got_signal);

        if got_signal {
            return;
        }
        try_restart_syscall(frame);
    }

    fn sys_rt_sigreturn(trap_frame: &mut TrapFrame) -> u64 {
        let frame = (trap_frame.rsp as usize - size_of::<u64>()) as *mut SigFrame;

        // 如果当前的rsp不来自用户态，则认为产生了错误（或被SROP攻击）
        if UserBufferWriter::new(frame, size_of::<SigFrame>(), true).is_err() {
            error!("rsp doesn't from user level");
            let _r = crate::ipc::kill::kill_process(
                ProcessManager::current_pcb().raw_pid(),
                Signal::SIGSEGV,
            )
            .map_err(|e| e.to_posix_errno());
            return trap_frame.rax;
        }
        let mut sigmask: SigSet = unsafe { (*frame).context.oldmask };
        set_current_blocked(&mut sigmask);
        // 从用户栈恢复sigcontext
        if !unsafe { &mut (*frame).context }.restore_sigcontext(trap_frame) {
            error!("unable to restore sigcontext");
            let _r = crate::ipc::kill::kill_process(
                ProcessManager::current_pcb().raw_pid(),
                Signal::SIGSEGV,
            )
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
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/signal.c#787
#[inline(never)]
fn handle_signal(
    sig: Signal,
    sigaction: &mut Sigaction,
    info: &SigInfo,
    oldset: &SigSet,
    frame: &mut TrapFrame,
) -> Result<i32, SystemError> {
    if unsafe { frame.syscall_nr() }.is_some() {
        if let Some(syscall_err) = unsafe { frame.syscall_error() } {
            match syscall_err {
                SystemError::ERESTARTNOHAND => {
                    frame.rax = SystemError::EINTR.to_posix_errno() as i64 as u64;
                }
                SystemError::ERESTARTSYS => {
                    if !sigaction.flags().contains(SigFlags::SA_RESTART) {
                        frame.rax = SystemError::EINTR.to_posix_errno() as i64 as u64;
                    } else {
                        frame.rax = frame.errcode;
                        frame.rip -= 2;
                    }
                }
                SystemError::ERESTART_RESTARTBLOCK => {
                    // 为了让带 SA_RESTART 的时序（例如 clock_nanosleep 相对睡眠）也能自动重启，
                    // 当 SA_RESTART 设置时，按 ERESTARTSYS 的语义处理；否则返回 EINTR。
                    if !sigaction.flags().contains(SigFlags::SA_RESTART) {
                        frame.rax = SystemError::EINTR.to_posix_errno() as i64 as u64;
                    } else {
                        frame.rax = frame.errcode;
                        frame.rip -= 2;
                    }
                }
                SystemError::ERESTARTNOINTR => {
                    frame.rax = frame.errcode;
                    frame.rip -= 2;
                }
                _ => {}
            }
        }
    }
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
            SaHandlerType::Default => {
                sig.handle_default();
                return Ok(0);
            }
            SaHandlerType::Customized(handler) => {
                // 如果handler位于内核空间
                if handler >= MMArch::USER_END_VADDR {
                    // 如果当前是SIGSEGV,则采用默认函数处理
                    if sig == Signal::SIGSEGV {
                        sig.handle_default();
                        return Ok(0);
                    } else {
                        error!("attempting  to execute a signal handler from kernel");
                        sig.handle_default();
                        return Err(SystemError::EINVAL);
                    }
                } else {
                    // 为了与Linux的兼容性，64位程序必须由用户自行指定restorer
                    if sigaction.flags().contains(SigFlags::SA_RESTORER) {
                        ret_code_ptr = sigaction.restorer().unwrap().data() as *mut c_void;
                    } else {
                        error!(
                            "pid-{:?} forgot to set SA_FLAG_RESTORER for signal {:?}",
                            ProcessManager::current_pcb().raw_pid(),
                            sig as i32
                        );
                        let r = crate::ipc::kill::kill_process(
                            ProcessManager::current_pcb().raw_pid(),
                            Signal::SIGSEGV,
                        );
                        if r.is_err() {
                            error!("In setup_sigcontext: generate SIGSEGV signal failed");
                        }
                        return Err(SystemError::EINVAL);
                    }
                    if sigaction.restorer().is_none() {
                        error!(
                            "restorer in process:{:?} is not defined",
                            ProcessManager::current_pcb().raw_pid()
                        );
                        return Err(SystemError::EINVAL);
                    }
                    temp_handler = handler.data() as *mut c_void;
                }
            }
            SaHandlerType::Ignore => {
                return Ok(0);
            }
            _ => {
                return Err(SystemError::EINVAL);
            }
        },
        SigactionType::SaSigaction(_) => {
            //TODO 这里应该是可以恢复栈的，等后续来做
            error!("trying to recover from sigaction type instead of handler");
            return Err(SystemError::EINVAL);
        }
    }
    let frame: *mut SigFrame = get_stack(trap_frame, size_of::<SigFrame>());

    // 要求这个frame的地址位于用户空间，因此进行校验
    let r: Result<UserBufferWriter<'_>, SystemError> =
        UserBufferWriter::new(frame, size_of::<SigFrame>(), true);
    if r.is_err() {
        // 如果地址区域位于内核空间，则直接报错
        // todo: 生成一个sigsegv
        let r = crate::ipc::kill::kill_process(
            ProcessManager::current_pcb().raw_pid(),
            Signal::SIGSEGV,
        );
        if r.is_err() {
            error!("In setup frame: generate SIGSEGV signal failed");
        }
        error!("In setup frame: access check failed");
        return Err(SystemError::EFAULT);
    }

    // 将siginfo拷贝到用户栈
    info.copy_posix_siginfo_to_user(unsafe { &mut ((*frame).info) as *mut PosixSigInfo })
        .map_err(|e| -> SystemError {
            let r = crate::ipc::kill::kill_process(
                ProcessManager::current_pcb().raw_pid(),
                Signal::SIGSEGV,
            );
            if r.is_err() {
                error!("In copy_posix_siginfo_to_user: generate SIGSEGV signal failed");
            }
            return e;
        })?;

    // todo: 拷贝处理程序备用栈的地址、大小、ss_flags

    unsafe {
        (*frame)
            .context
            .setup_sigcontext(oldset, trap_frame)
            .map_err(|e: SystemError| -> SystemError {
                let r = crate::ipc::kill::kill_process(
                    ProcessManager::current_pcb().raw_pid(),
                    Signal::SIGSEGV,
                );
                if r.is_err() {
                    error!("In setup_sigcontext: generate SIGSEGV signal failed");
                }
                return e;
            })?
    };

    unsafe {
        // 在开头检验过sigaction.restorer是否为空了，实际上libc会保证 restorer始终不为空
        (*frame).ret_code_ptr = ret_code_ptr;
    }

    unsafe { (*frame).handler = temp_handler };
    // 传入信号处理函数的第一个参数
    trap_frame.rdi = sig as u64;
    trap_frame.rsi = unsafe { &(*frame).info as *const PosixSigInfo as u64 };
    trap_frame.rsp = frame as u64;
    trap_frame.rip = unsafe { (*frame).handler as u64 };
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
