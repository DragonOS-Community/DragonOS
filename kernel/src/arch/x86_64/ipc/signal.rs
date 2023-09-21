use core::{ffi::c_void, mem::size_of};

use alloc::sync::Arc;

use crate::{
    arch::{
        interrupt::TrapFrame,
        process::table::{USER_CS, USER_DS},
        CurrentIrqArch,
    },
    exception::InterruptArch,
    include::bindings::bindings::USER_MAX_LINEAR_ADDR,
    ipc::{
        signal::{get_signal_to_deliver, set_current_sig_blocked},
        signal_types::{
            SigContext, SigFlags, SigFrame, SigInfo, SigSet, SigType, Sigaction, SigactionType,
            SignalNumber,
        },
    },
    kdebug, kerror,
    process::{Pid, ProcessManager},
    syscall::{user_access::UserBufferWriter, SystemError},
};

/// 最大支持的信号数量
pub const _NSIG: usize = 64;
/// 实时信号的最小值
pub const SIGRTMIN: usize = 32;
/// 信号处理的栈的栈指针的最小对齐数量
pub const STACK_ALIGN: u64 = 16;

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
    let user_buffer = r.unwrap();

    match ka.action() {
        SigactionType::SaHandler(handler) => {
            if handler.is_none() {
                kerror!("In setup frame: handler is None");
                return Err(SystemError::EINVAL);
            }
            unsafe {
                (*frame).arg0 = sig as u64;
                (*frame).arg1 = &((*frame).info) as *const SigInfo as usize;
                (*frame).arg2 = &((*frame).context) as *const SigContext as usize;
                (*frame).handler = handler.unwrap() as usize as *mut c_void;
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
    err |= copy_siginfo_to_user(unsafe { &mut (*frame).info }, info).unwrap_or(1);

    // todo: 拷贝处理程序备用栈的地址、大小、ss_flags

    err |= setup_sigcontext(unsafe { &mut (*frame).context }, oldset, &trap_frame).unwrap_or(1);

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

/// @brief 设置目标的sigcontext
///
/// @param context 要被设置的目标sigcontext
/// @param mask 要被暂存的信号mask标志位
/// @param regs 进入信号处理流程前，Restore all要弹出的内核栈栈帧
fn setup_sigcontext(
    context: &mut SigContext,
    mask: &SigSet,
    frame: &TrapFrame,
) -> Result<i32, SystemError> {
    //TODO 引入线程后补上
    // let current_thread = ProcessManager::current_pcb().thread;

    context.oldmask = *mask;
    context.frame = frame.clone();
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
fn restore_sigcontext(context: &SigContext, frame: &mut TrapFrame) -> bool {
    let guard = ProcessManager::current_pcb();
    let mut arch_info = guard.arch_info();
    *frame = (*context).frame;

    // (*current_thread).trap_num = (*context).trap_num;
    *arch_info.cr2_mut() = (*context).cr2 as usize;
    // (*current_thread).err_code = (*context).err_code;
    // 如果当前进程有fpstate，则将其恢复到pcb的fp_state中
    ProcessManager::current_pcb().arch_info().restore_fp_state();
    return true;
}

#[inline(always)]
fn get_stack(_ka: &Sigaction, frame: &TrapFrame, size: usize) -> *mut SigFrame {
    // 默认使用 用户栈的栈顶指针-128字节的红区-sigframe的大小
    let mut rsp: usize = (frame.rsp as usize) - 128 - size;
    // 按照要求进行对齐
    rsp &= (-(STACK_ALIGN as i64)) as usize;
    return rsp as *mut SigFrame;
}

/// @brief 将siginfo结构体拷贝到用户栈
fn copy_siginfo_to_user(to: *mut SigInfo, from: &SigInfo) -> Result<i32, SystemError> {
    // 验证目标地址是否为用户空间
    let mut user_buffer = UserBufferWriter::new(to, size_of::<SigInfo>(), true)?;

    let retval: Result<i32, SystemError> = Ok(0);

    // todo: 将这里按照si_code的类型来分别拷贝不同的信息。
    // 这里参考linux-2.6.39  网址： http://opengrok.ringotek.cn/xref/linux-2.6.39/arch/ia64/kernel/signal.c#137

    //     pub struct SigInfo {
    //     sig_no: i32,
    //     code: i32,
    //     errno: i32,
    //     reserved: u32,
    //     sig_type: SigType,
    // }
    // 因此下面的偏移量就是 i32+i32+i32+u32
    let pid = match from.sig_type() {
        SigType::Kill(pid) => pid,
    };
    user_buffer.copy_one_to_user::<Pid>(&pid, size_of::<i32>() * 3 + size_of::<u32>());

    return retval;
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
    if restore_sigcontext(unsafe { &mut (*frame).context }, trap_frame) == false {
        // todo：这里改为生成一个sigsegv
        // 退出进程
        ProcessManager::exit(SignalNumber::SIGSEGV as usize);
    }

    // 由于系统调用的返回值会被系统调用模块被存放在rax寄存器，因此，为了还原原来的那个系统调用的返回值，我们需要在这里返回恢复后的rax的值
    return trap_frame.rax;
}
