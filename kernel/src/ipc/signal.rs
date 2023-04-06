use core::{
    ffi::c_void,
    intrinsics::size_of,
    ptr::{null_mut, read_volatile},
    sync::atomic::compiler_fence,
};

use crate::{
    arch::{
        asm::{bitops::ffz, current::current_pcb, ptrace::user_mode},
        fpu::FpState,
        interrupt::sti,
    },
    include::bindings::bindings::{
        pid_t, process_control_block, process_do_exit, process_find_pcb_by_pid, pt_regs,
        spinlock_t, verify_area, NULL, PF_EXITING,
        PF_KTHREAD, PF_SIGNALED, PF_WAKEKILL, PROC_INTERRUPTIBLE, USER_CS, USER_DS,
        USER_MAX_LINEAR_ADDR,
    },
    ipc::signal_types::{sigset_add, user_sigaction},
    kBUG, kdebug, kerror, kwarn,
    libs::{
        ffi_convert::FFIBind2Rust,
        spinlock::{
            spin_is_locked, spin_lock_irq, spin_lock_irqsave, spin_unlock_irq,
            spin_unlock_irqrestore,
        },
    },
    process::{
        pid::PidType,
        process::{process_is_stopped, process_kick, process_wake_up_state},
    }, syscall::SystemError,
};

use super::signal_types::{
    si_code_val, sig_is_member, sigaction, sigaction__union_u, sigcontext, sigframe,
    sighand_struct, siginfo, signal_struct, sigpending, sigset_clear, sigset_del, sigset_delmask,
    sigset_equal, sigset_init, sigset_t, SigQueue, SignalNumber, MAX_SIG_NUM, SA_ALL_FLAGS,
    SA_FLAG_DFL, SA_FLAG_IGN, SA_FLAG_IMMUTABLE, SA_FLAG_RESTORER, STACK_ALIGN, USER_SIG_DFL,
    USER_SIG_IGN, _NSIG_U64_CNT,
};

use super::signal_types::{__siginfo_union, __siginfo_union_data};

/// 默认信号处理程序占位符（用于在sighand结构体中的action数组中占位）
pub static DEFAULT_SIGACTION: sigaction = sigaction {
    _u: sigaction__union_u {
        _sa_handler: NULL as u64,
    },
    sa_flags: SA_FLAG_DFL,
    sa_mask: 0,
    sa_restorer: NULL as u64,
};

/// 默认的“忽略信号”的sigaction
#[allow(dead_code)]
pub static DEFAULT_SIGACTION_IGNORE: sigaction = sigaction {
    _u: sigaction__union_u {
        _sa_handler: NULL as u64,
    },
    sa_flags: SA_FLAG_IGN,
    sa_mask: 0,
    sa_restorer: NULL as u64,
};

/// @brief kill系统调用，向指定的进程发送信号
/// @param regs->r8 pid 要接收信号的进程id
/// @param regs->r9 sig 信号
#[no_mangle]
pub extern "C" fn sys_kill(regs: &pt_regs) -> u64 {
    let pid: pid_t = regs.r8 as pid_t;
    let sig: SignalNumber = SignalNumber::from(regs.r9 as i32);

    if sig == SignalNumber::INVALID {
        // 传入的signal数值不合法
        kwarn!("Not a valid signal number");
        return SystemError::EINVAL.to_posix_errno() as u64;
    }

    // 初始化signal info
    let mut info = siginfo {
        _sinfo: __siginfo_union {
            data: __siginfo_union_data {
                si_signo: sig as i32,
                si_code: si_code_val::SI_USER as i32,
                si_errno: 0,
                reserved: 0,
                _sifields: super::signal_types::__sifields {
                    _kill: super::signal_types::__sifields__kill { _pid: pid },
                },
            },
        },
    };
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    let retval = signal_kill_something_info(sig, Some(&mut info), pid);
    let x;
    if retval.is_ok() {
        x = retval.unwrap();
    } else {
        x = retval.unwrap_err().to_posix_errno();
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    return x as u64;
}

/// 通过kill的方式向目标进程发送信号
/// @param sig 要发送的信号
/// @param info 要发送的信息
/// @param pid 进程id（目前只支持pid>0)
fn signal_kill_something_info(
    sig: SignalNumber,
    info: Option<&mut siginfo>,
    pid: pid_t,
) -> Result<i32, SystemError> {
    // 暂时不支持特殊的kill操作
    if pid <= 0 {
        kwarn!("Kill operation not support: pid={}", pid);
        return Err(SystemError::ENOTSUP);
    }

    // kill单个进程
    return signal_kill_proc_info(sig, info, pid);
}

fn signal_kill_proc_info(
    sig: SignalNumber,
    info: Option<&mut siginfo>,
    pid: pid_t,
) -> Result<i32, SystemError> {
    let mut retval = Err(SystemError::ESRCH);

    // step1: 当进程管理模块拥有pcblist_lock之后，对其加锁

    // step2: 根据pid找到pcb
    let pcb = unsafe { process_find_pcb_by_pid(pid).as_mut() };

    if pcb.is_none() {
        kwarn!("No such process.");
        return retval;
    }

    // println!("Target pcb = {:?}", pcb.as_ref().unwrap());
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    // step3: 调用signal_send_sig_info函数，发送信息
    retval = signal_send_sig_info(sig, info, pcb.unwrap());
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    // step4: 解锁
    return retval;
}

/// @brief 验证信号的值是否在范围内
#[inline]
fn verify_signal(sig: SignalNumber) -> bool {
    return if (sig as i32) <= MAX_SIG_NUM {
        true
    } else {
        false
    };
}

/// @brief 在发送信号给指定的进程前，做一些权限检查. 检查是否有权限发送
/// @param sig 要发送的信号
/// @param info 要发送的信息
/// @param target_pcb 信号的接收者
fn signal_send_sig_info(
    sig: SignalNumber,
    info: Option<&mut siginfo>,
    target_pcb: &mut process_control_block,
) -> Result<i32, SystemError> {
    // kdebug!("signal_send_sig_info");
    // 检查sig是否符合要求，如果不符合要求，则退出。
    if !verify_signal(sig) {
        return Err(SystemError::EINVAL);
    }

    // 信号符合要求，可以发送

    let mut retval = Err(SystemError::ESRCH);
    let mut flags: u64 = 0;
    // 如果上锁成功，则发送信号
    if !lock_process_sighand(target_pcb, &mut flags).is_none() {
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        // 发送信号
        retval = send_signal_locked(sig, info, target_pcb, PidType::PID);
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        // kdebug!("flags=0x{:016x}", flags);
        // 对sighand放锁
        unlock_process_sighand(target_pcb, flags);
    }
    return retval;
}

/// @brief 对pcb的sighand结构体中的siglock进行加锁，并关闭中断
/// @param pcb 目标pcb
/// @param flags 用来保存rflags的变量
/// @return 指向sighand_struct的可变引用
fn lock_process_sighand<'a>(
    pcb: &'a mut process_control_block,
    flags: &mut u64,
) -> Option<&'a mut sighand_struct> {
    // kdebug!("lock_process_sighand");

    let sighand_ptr = sighand_struct::convert_mut(unsafe { &mut *pcb.sighand });
    // kdebug!("sighand_ptr={:?}", &sighand_ptr);
    if !sighand_ptr.is_some() {
        kBUG!("Sighand ptr of process {pid} is NULL!", pid = pcb.pid);
        return None;
    }

    let lock = { &mut sighand_ptr.unwrap().siglock };

    spin_lock_irqsave(lock, flags);
    let ret = unsafe { ((*pcb).sighand as *mut sighand_struct).as_mut() };

    return ret;
}

/// @brief 对pcb的sighand结构体中的siglock进行放锁，并恢复之前存储的rflags
/// @param pcb 目标pcb
/// @param flags 用来保存rflags的变量，将这个值恢复到rflags寄存器中
fn unlock_process_sighand(pcb: &mut process_control_block, flags: u64) {
    let lock = unsafe { &mut (*pcb.sighand).siglock };

    spin_unlock_irqrestore(lock, &flags);
}

/// @brief 判断是否需要强制发送信号，然后发送信号
/// 注意，进入该函数前，我们应当对pcb.sighand.siglock加锁。
///
/// @return SystemError 错误码
fn send_signal_locked(
    sig: SignalNumber,
    info: Option<&mut siginfo>,
    pcb: &mut process_control_block,
    pt: PidType,
) -> Result<i32, SystemError> {
    // 是否强制发送信号
    let mut force_send = false;
    // signal的信息为空
    if info.is_none() {
        // todo: 判断signal是否来自于一个祖先进程的namespace，如果是，则强制发送信号
    } else {
        force_send = unsafe { info.as_ref().unwrap()._sinfo.data.si_code }
            == (si_code_val::SI_KERNEL as i32);
    }

    // kdebug!("force send={}", force_send);

    return __send_signal_locked(sig, info, pcb, pt, force_send);
}

/// @brief 发送信号
/// 注意，进入该函数前，我们应当对pcb.sighand.siglock加锁。
///
/// @param sig 信号
/// @param _info 信号携带的信息
/// @param pcb 目标进程的pcb
/// @param pt siginfo结构体中，pid字段代表的含义
/// @return SystemError 错误码
fn __send_signal_locked(
    sig: SignalNumber,
    info: Option<&mut siginfo>,
    pcb: &mut process_control_block,
    pt: PidType,
    _force_send: bool,
) -> Result<i32, SystemError> {
    // kdebug!("__send_signal_locked");

    // 判断该进入该函数时，是否已经持有了锁
    assert!(spin_is_locked(unsafe { &(*pcb.sighand).siglock }));

    let _pending: Option<&mut sigpending> = sigpending::convert_mut(&mut pcb.sig_pending);
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    // 如果是kill或者目标pcb是内核线程，则无需获取sigqueue，直接发送信号即可
    if sig == SignalNumber::SIGKILL || (pcb.flags & (PF_KTHREAD as u64)) != 0 {
        complete_signal(sig, pcb, pt);
    } else {
        // 如果是其他信号，则加入到sigqueue内，然后complete_signal
        let mut q: siginfo;
        match info {
            Some(x) => {
                // 已经显式指定了siginfo，则直接使用它。
                q = x.clone();
            }
            None => {
                // 不需要显示指定siginfo，因此设置为默认值
                q = siginfo::new(sig, 0, si_code_val::SI_USER);
                q._sinfo.data._sifields._kill._pid = current_pcb().pid;
            }
        }

        let sq: &mut SigQueue = SigQueue::from_c_void(current_pcb().sig_pending.sigqueue);
        sq.q.push(q);
        complete_signal(sig, pcb, pt);
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    return Ok(0);
}

/// @brief 将信号添加到目标进程的sig_pending。在引入进程组后，本函数还将负责把信号传递给整个进程组。
///
/// @param sig 信号
/// @param pcb 目标pcb
/// @param pt siginfo结构体中，pid字段代表的含义
fn complete_signal(sig: SignalNumber, pcb: &mut process_control_block, pt: PidType) {
    // kdebug!("complete_signal");

    // todo: 将信号产生的消息通知到正在监听这个信号的进程（引入signalfd之后，在这里调用signalfd_notify)
    // 将这个信号加到目标进程的sig_pending中
    sigset_add(
        sigset_t::convert_mut(&mut pcb.sig_pending.signal).unwrap(),
        sig,
    );
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    // ===== 寻找需要wakeup的目标进程 =====
    // 备注：由于当前没有进程组的概念，每个进程只有1个对应的线程，因此不需要通知进程组内的每个进程。
    //      todo: 当引入进程组的概念后，需要完善这里，使得它能寻找一个目标进程来唤醒，接着执行信号处理的操作。

    let _signal: Option<&mut signal_struct> = signal_struct::convert_mut(pcb.signal);

    let mut _target: Option<&mut process_control_block> = None;

    // 判断目标进程是否想接收这个信号
    if wants_signal(sig, pcb) {
        _target = Some(pcb);
    } else if pt == PidType::PID {
        /*
         * There is just one thread and it does not need to be woken.
         * It will dequeue unblocked signals before it runs again.
         */
        return;
    } else {
        /*
         * Otherwise try to find a suitable thread.
         * 由于目前每个进程只有1个线程，因此当前情况可以返回。信号队列的dequeue操作不需要考虑同步阻塞的问题。
         */
        return;
    }

    // todo:引入进程组后，在这里挑选一个进程来唤醒，让它执行相应的操作。
    // todo!();
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    // todo: 到这里，信号已经被放置在共享的pending队列中，我们在这里把目标进程唤醒。
    if _target.is_some() {
        signal_wake_up(pcb, sig == SignalNumber::SIGKILL);
    }
}

/// @brief 本函数用于检测指定的进程是否想要接收SIG这个信号。
/// 当我们对于进程组中的所有进程都运行了这个检查之后，我们将可以找到组内愿意接收信号的进程。
/// 这么做是为了防止我们把信号发送给了一个正在或已经退出的进程，或者是不响应该信号的进程。
#[inline]
fn wants_signal(sig: SignalNumber, pcb: &process_control_block) -> bool {
    // 如果改进程屏蔽了这个signal，则不能接收
    if sig_is_member(sigset_t::convert_ref(&pcb.sig_blocked).unwrap(), sig) {
        return false;
    }

    // 如果进程正在退出，则不能接收信号
    if (pcb.flags & (PF_EXITING as u64)) > 0 {
        return false;
    }

    if sig == SignalNumber::SIGKILL {
        return true;
    }

    if process_is_stopped(pcb) {
        return false;
    }

    // todo: 检查目标进程是否正在一个cpu上执行，如果是，则返回true，否则继续检查下一项

    // 检查目标进程是否有信号正在等待处理，如果是，则返回false，否则返回true
    return !has_sig_pending(pcb);
}

/// @brief 判断signal的处理是否可能使得整个进程组退出
/// @return true 可能会导致退出（不一定）
#[allow(dead_code)]
#[inline]
fn sig_fatal(pcb: &process_control_block, sig: SignalNumber) -> bool {
    let handler = unsafe {
        sighand_struct::convert_ref(pcb.sighand).unwrap().action[(sig as usize) - 1]
            ._u
            ._sa_handler
    };

    // 如果handler是空，采用默认函数，signal处理可能会导致进程退出。
    if handler == NULL.into() {
        return true;
    } else {
        return false;
    }

    // todo: 参照linux的sig_fatal实现完整功能
}

/// @brief 判断某个进程是否有信号正在等待处理
#[inline]
fn has_sig_pending(pcb: &process_control_block) -> bool {
    let ptr = &sigpending::convert_ref(&(*pcb).sig_pending).unwrap().signal;
    if unsafe { read_volatile(ptr) } != 0 {
        return true;
    } else {
        return false;
    }
}

#[inline]
fn signal_wake_up(pcb: &mut process_control_block, fatal: bool) {
    // kdebug!("signal_wake_up");
    let mut state: u64 = 0;
    if fatal {
        state = PF_WAKEKILL as u64;
    }
    signal_wake_up_state(pcb, state);
}

fn signal_wake_up_state(pcb: &mut process_control_block, state: u64) {
    assert!(spin_is_locked(&unsafe { (*pcb.sighand).siglock }));
    // todo: 设置线程结构体的标志位为TIF_SIGPENDING
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    // 如果目标进程已经在运行，则发起一个ipi，使得它陷入内核
    if !process_wake_up_state(pcb, state | (PROC_INTERRUPTIBLE as u64)) {
        process_kick(pcb);
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

/// @brief 信号处理函数。该函数在进程退出内核态的时候会被调用，且调用前会关闭中断。
#[no_mangle]
pub extern "C" fn do_signal(regs: &mut pt_regs) {
    // 检查sigpending是否为0
    if current_pcb().sig_pending.signal == 0 || (!user_mode(regs)) {
        // 若没有正在等待处理的信号，或者将要返回到的是内核态，则启用中断，然后返回
        sti();
        return;
    }

    // 做完上面的检查后，开中断
    sti();

    let oldset = current_pcb().sig_blocked;
    loop {
        let (sig_number, info, ka) = get_signal_to_deliver(regs.clone());
        // 所有的信号都处理完了
        if sig_number == SignalNumber::INVALID {
            return;
        }
        kdebug!(
            "To handle signal [{}] for pid:{}",
            sig_number as i32,
            current_pcb().pid
        );
        let res = handle_signal(sig_number, ka.unwrap(), &info.unwrap(), &oldset, regs);
        if res.is_err() {
            kerror!(
                "Error occurred when handling signal: {}, pid={}, errcode={:?}",
                sig_number as i32,
                current_pcb().pid,
                res.unwrap_err()
            );
        }
    }
}

/// @brief 获取要被发送的信号的signumber, siginfo, 以及对应的sigaction结构体
fn get_signal_to_deliver(
    _regs: pt_regs,
) -> (
    SignalNumber,
    Option<siginfo>,
    Option<&'static mut sigaction>,
) {
    let mut info: Option<siginfo>;
    let ka: Option<&mut sigaction>;
    let mut sig_number;
    let sighand: &mut sighand_struct;

    {
        let _tmp = sighand_struct::convert_mut(current_pcb().sighand);
        if let Some(i) = _tmp {
            sighand = i;
        } else {
            panic!("Sighand is NULL! pid={}", current_pcb().pid);
        }
    }

    spin_lock_irq(&mut sighand.siglock);
    loop {
        (sig_number, info) =
            dequeue_signal(sigset_t::convert_mut(&mut current_pcb().sig_blocked).unwrap());

        // 如果信号非法，则直接返回
        if sig_number == SignalNumber::INVALID {
            spin_unlock_irq(unsafe { (&mut (*current_pcb().sighand).siglock) as *mut spinlock_t });
            return (sig_number, None, None);
        }

        // 获取指向sigaction结构体的引用
        let hand = sighand_struct::convert_mut(current_pcb().sighand).unwrap();
        // kdebug!("hand=0x{:018x}", hand as *const sighand_struct as usize);
        let tmp_ka = &mut hand.action[sig_number as usize - 1];

        // 如果当前动作是忽略这个信号，则不管它了。
        if (tmp_ka.sa_flags & SA_FLAG_IGN) != 0 {
            continue;
        } else if (tmp_ka.sa_flags & SA_FLAG_DFL) == 0 {
            // 当前不采用默认的信号处理函数
            ka = Some(tmp_ka);
            break;
        }
        kdebug!(
            "Use default handler to handle signal [{}] for pid {}",
            sig_number as i32,
            current_pcb().pid
        );
        // ===== 经过上面的判断，如果能走到这一步，就意味着我们采用默认的信号处理函数来处理这个信号 =====
        spin_unlock_irq(&mut sighand.siglock);
        // 标记当前进程由于信号而退出
        current_pcb().flags |= PF_SIGNALED as u64;

        // 执行进程的退出动作
        unsafe { process_do_exit(info.unwrap()._sinfo.data.si_signo as u64) };
        /* NOT REACHED 这部分代码将不会到达 */
    }
    spin_unlock_irq(&mut sighand.siglock);
    return (sig_number, info, ka);
}

/// @brief 从当前进程的sigpending中取出下一个待处理的signal，并返回给调用者。（调用者应当处理这个信号）
/// 请注意，进入本函数前，当前进程应当持有current_pcb().sighand.siglock
fn dequeue_signal(sig_mask: &mut sigset_t) -> (SignalNumber, Option<siginfo>) {
    // kdebug!("dequeue signal");
    // 获取下一个要处理的信号的编号
    let sig = next_signal(
        sigpending::convert_ref(&(current_pcb().sig_pending)).unwrap(),
        sig_mask,
    );

    let info: Option<siginfo>;
    if sig != SignalNumber::INVALID {
        // 如果下一个要处理的信号是合法的，则收集其siginfo
        info = Some(collect_signal(
            sig,
            sigpending::convert_mut(&mut current_pcb().sig_pending).unwrap(),
        ));
    } else {
        info = None;
    }

    // 当一个进程具有多个线程之后，在这里需要重新计算线程的flag中的TIF_SIGPENDING位
    recalc_sigpending();
    return (sig, info);
}

/// @brief 获取下一个要处理的信号（sig number越小的信号，优先级越高）
///
/// @param pending 等待处理的信号
/// @param sig_mask 屏蔽了的信号
/// @return i32 下一个要处理的信号的number. 如果为0,则无效
fn next_signal(pending: &sigpending, sig_mask: &sigset_t) -> SignalNumber {
    let mut sig = SignalNumber::INVALID;

    let s = pending.signal;
    let m = *sig_mask;

    // 获取第一个待处理的信号的号码
    let x = s & (!m);
    if x != 0 {
        sig = SignalNumber::from(ffz(!x) + 1);
        return sig;
    }

    // 暂时只支持64种信号信号
    assert_eq!(_NSIG_U64_CNT, 1);

    return sig;
}

/// @brief 当一个进程具有多个线程之后，在这里需要重新计算线程的flag中的TIF_SIGPENDING位
fn recalc_sigpending() {
    // todo:
}

/// @brief 收集信号的信息
///
/// @param sig 要收集的信号的信息
/// @param pending 信号的排队等待标志
/// @return siginfo 信号的信息
fn collect_signal(sig: SignalNumber, pending: &mut sigpending) -> siginfo {
    let (info, still_pending) = unsafe { pending.queue.as_mut() }
        .unwrap()
        .find_and_delete(sig);

    // 如果没有仍在等待的信号，则清除pending位
    if !still_pending {
        sigset_del(&mut pending.signal, sig);
    }

    if info.is_some() {
        return info.unwrap();
    } else {
        // 信号不在sigqueue中，这意味着当前信号是来自快速路径，因此直接把siginfo设置为0即可。
        let mut ret = siginfo::new(sig, 0, si_code_val::SI_USER);
        ret._sinfo.data._sifields._kill._pid = 0;
        return ret;
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
    ka: &mut sigaction,
    info: &siginfo,
    oldset: &sigset_t,
    regs: &mut pt_regs,
) -> Result<i32, SystemError> {
    // 设置栈帧
    let retval = setup_frame(sig, ka, info, oldset, regs);
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
    ka: &mut sigaction,
    info: &siginfo,
    oldset: &sigset_t,
    regs: &mut pt_regs,
) -> Result<i32, SystemError> {
    let mut err = 0;
    let frame: *mut sigframe = get_stack(ka, &regs, size_of::<sigframe>());
    // kdebug!("frame=0x{:016x}", frame as usize);
    // 要求这个frame的地址位于用户空间，因此进行校验
    let access_check_ok = unsafe { verify_area(frame as u64, size_of::<sigframe>() as u64) };
    if !access_check_ok {
        // 如果地址区域位于内核空间，则直接报错
        // todo: 生成一个sigsegv
        kerror!("In setup frame: access check failed");
        return Err(SystemError::EPERM);
    }

    unsafe {
        (*frame).arg0 = sig as u64;
        (*frame).arg1 = &((*frame).info) as *const siginfo as usize;
        (*frame).arg2 = &((*frame).context) as *const sigcontext as usize;
        (*frame).handler = ka._u._sa_handler as usize as *mut c_void;
    }

    // 将当前进程的fp_state拷贝到用户栈
    if current_pcb().fp_state != null_mut() {
        unsafe {
            let fp_state: &mut FpState = (current_pcb().fp_state as usize as *mut FpState)
                .as_mut()
                .unwrap();
            (*frame).context.sc_stack.fpstate = *fp_state;
            // 保存完毕后，清空fp_state，以免下次save的时候，出现SIMD exception
            fp_state.clear();
        }
    }
    // 将siginfo拷贝到用户栈
    err |= copy_siginfo_to_user(unsafe { &mut (*frame).info }, info).unwrap_or(1);

    // todo: 拷贝处理程序备用栈的地址、大小、ss_flags

    err |= setup_sigcontext(unsafe { &mut (*frame).context }, oldset, &regs).unwrap_or(1);

    // 为了与Linux的兼容性，64位程序必须由用户自行指定restorer
    if ka.sa_flags & SA_FLAG_RESTORER != 0 {
        unsafe {
            (*frame).ret_code_ptr = ka.sa_restorer as usize as *mut c_void;
        }
    } else {
        kerror!(
            "pid-{} forgot to set SA_FLAG_RESTORER for signal {}",
            current_pcb().pid,
            sig as i32
        );
        err = 1;
    }
    if err != 0 {
        // todo: 在这里生成一个sigsegv,然后core dump
        //临时解决方案：退出当前进程
        unsafe{
            process_do_exit(1);
        }
    }
    // 传入信号处理函数的第一个参数
    regs.rdi = sig as u64;
    regs.rsi = unsafe { &(*frame).info as *const siginfo as u64 };
    regs.rsp = frame as u64;
    regs.rip = unsafe { ka._u._sa_handler };

    // todo: 传入新版的sa_sigaction的处理函数的第三个参数

    // 如果handler位于内核空间
    if regs.rip >= USER_MAX_LINEAR_ADDR {
        // 如果当前是SIGSEGV,则采用默认函数处理
        if sig == SignalNumber::SIGSEGV {
            ka.sa_flags |= SA_FLAG_DFL;
        }

        // 将rip设置为0
        regs.rip = 0;
    }

    // 设置cs和ds寄存器
    regs.cs = (USER_CS | 0x3) as u64;
    regs.ds = (USER_DS | 0x3) as u64;
    
    return if err == 0 { Ok(0) } else { Err(SystemError::EPERM) };
}

#[inline(always)]
fn get_stack(_ka: &sigaction, regs: &pt_regs, size: usize) -> *mut sigframe {
    // 默认使用 用户栈的栈顶指针-128字节的红区-sigframe的大小
    let mut rsp: usize = (regs.rsp as usize) - 128 - size;
    // 按照要求进行对齐
    rsp &= (-(STACK_ALIGN as i64)) as usize;
    return rsp as *mut sigframe;
}

/// @brief 将siginfo结构体拷贝到用户栈
fn copy_siginfo_to_user(to: *mut siginfo, from: &siginfo) -> Result<i32, SystemError> {
    // 验证目标地址是否为用户空间
    if unsafe { !verify_area(to as u64, size_of::<siginfo>() as u64) } {
        // 如果目标地址空间不为用户空间，则直接返回错误码 -EPERM
        return Err(SystemError::EPERM);
    }

    let retval: Result<i32, SystemError> = Ok(0);

    // todo: 将这里按照si_code的类型来分别拷贝不同的信息。
    // 这里参考linux-2.6.39  网址： http://opengrok.ringotek.cn/xref/linux-2.6.39/arch/ia64/kernel/signal.c#137

    unsafe {
        (*to)._sinfo.data._sifields._kill._pid = from._sinfo.data._sifields._kill._pid;
    }

    return retval;
}

/// @brief 设置目标的sigcontext
///
/// @param context 要被设置的目标sigcontext
/// @param mask 要被暂存的信号mask标志位
/// @param regs 进入信号处理流程前，Restore all要弹出的内核栈栈帧
fn setup_sigcontext(context: &mut sigcontext, mask: &sigset_t, regs: &pt_regs) -> Result<i32, SystemError> {
    let current_thread = current_pcb().thread;

    context.oldmask = *mask;
    context.regs = regs.clone();
    context.trap_num = unsafe { (*current_thread).trap_num };
    context.err_code = unsafe { (*current_thread).err_code };
    context.cr2 = unsafe { (*current_thread).cr2 };
    return Ok(0);
}

/// @brief 将指定的sigcontext恢复到当前进程的内核栈帧中,并将当前线程结构体的几个参数进行恢复
///
/// @param context 要被恢复的context
/// @param regs 目标栈帧（也就是把context恢复到这个栈帧中）
///
/// @return bool true -> 成功恢复
///              false -> 执行失败
fn restore_sigcontext(context: *const sigcontext, regs: &mut pt_regs) -> bool {
    let mut current_thread = current_pcb().thread;
    unsafe {
        *regs = (*context).regs;

        (*current_thread).trap_num = (*context).trap_num;
        (*current_thread).cr2 = (*context).cr2;
        (*current_thread).err_code = (*context).err_code;

        // 如果当前进程有fpstate，则将其恢复到pcb的fp_state中
        *(current_pcb().fp_state as usize as *mut FpState) = (*context).sc_stack.fpstate;
    }

    return true;
}

/// @brief 刷新指定进程的sighand的sigaction，将满足条件的sigaction恢复为Default
///     除非某个信号被设置为ignore且force_default为false，否则都不会将其恢复
///
/// @param pcb 要被刷新的pcb
/// @param force_default 是否强制将sigaction恢复成默认状态
pub fn flush_signal_handlers(pcb: *mut process_control_block, force_default: bool) {
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    let action = unsafe { &mut (*(*pcb).sighand).action };
    for ka in action.iter_mut() {
        if force_default || (ka.sa_flags != SA_FLAG_IGN) {
            ka.sa_flags = SA_FLAG_DFL;
            ka._u._sa_handler = None;
        }
        // 清除flags中，除了DFL和IGN以外的所有标志
        ka.sa_flags &= SA_FLAG_DFL | SA_FLAG_IGN;
        ka.sa_restorer = None;
        sigset_clear(&mut ka.sa_mask);
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

/// @brief 用户程序用于设置信号处理动作的函数（遵循posix2008）
///
/// @param regs->r8 signumber 信号的编号
/// @param regs->r9 act 新的，将要被设置的sigaction
/// @param regs->r10 oact 返回给用户的原本的sigaction（内核将原本的sigaction的值拷贝给这个地址）
///
/// @return int 错误码
#[no_mangle]
pub extern "C" fn sys_sigaction(regs: &mut pt_regs) -> u64 {
    // 请注意：用户态传进来的user_sigaction结构体类型，请注意，这个结构体与内核实际的不一样
    let act = regs.r9 as usize as *mut user_sigaction;
    let mut old_act = regs.r10 as usize as *mut user_sigaction;
    let mut new_ka: sigaction = Default::default();
    let mut old_ka: sigaction = Default::default();

    // 如果传入的，新的sigaction不为空
    if !act.is_null() {
        // 如果参数的范围不在用户空间，则返回错误
        if unsafe { !verify_area(act as usize as u64, size_of::<sigaction>() as u64) } {
            return SystemError::EFAULT.to_posix_errno() as u64;
        }
        let mask: sigset_t = unsafe { (*act).sa_mask };
        let _input_sah = unsafe { (*act).sa_handler as u64 };
        // kdebug!("_input_sah={}", _input_sah);
        match _input_sah {
            USER_SIG_DFL | USER_SIG_IGN => {
                if _input_sah == USER_SIG_DFL {
                    new_ka = DEFAULT_SIGACTION;
                    new_ka.sa_flags =
                        (unsafe { (*act).sa_flags } & (!(SA_FLAG_DFL | SA_FLAG_IGN))) | SA_FLAG_DFL;
                } else {
                    new_ka = DEFAULT_SIGACTION_IGNORE;
                    new_ka.sa_flags =
                        (unsafe { (*act).sa_flags } & (!(SA_FLAG_DFL | SA_FLAG_IGN))) | SA_FLAG_IGN;
                }

                let sar = unsafe { (*act).sa_restorer };
                new_ka.sa_restorer = sar as u64;
            }
            _ => {
                // 从用户空间获得sigaction结构体
                new_ka = sigaction {
                    _u: sigaction__union_u {
                        _sa_handler: unsafe { (*act).sa_handler as u64 },
                    },
                    sa_flags: unsafe { (*act).sa_flags },
                    sa_mask: sigset_t::default(),
                    sa_restorer: unsafe { (*act).sa_restorer as u64 },
                };
            }
        }
        // kdebug!("new_ka={:?}", new_ka);
        // 如果用户手动给了sa_restorer，那么就置位SA_FLAG_RESTORER，否则报错。（用户必须手动指定restorer）
        if new_ka.sa_restorer != NULL as u64 {
            new_ka.sa_flags |= SA_FLAG_RESTORER;
        } else {
            kwarn!(
                "pid:{}: in sys_sigaction: User must manually sprcify a sa_restorer for signal {}.",
                current_pcb().pid,
                regs.r8.clone()
            );
        }
        sigset_init(&mut new_ka.sa_mask, mask);
    }

    let sig = SignalNumber::from(regs.r8 as i32);
    // 如果给出的信号值不合法
    if sig == SignalNumber::INVALID {
        return SystemError::EINVAL.to_posix_errno() as u64;
    }

    let retval = do_sigaction(
        sig,
        if act.is_null() {
            None
        } else {
            Some(&mut new_ka)
        },
        if old_act.is_null() {
            None
        } else {
            Some(&mut old_ka)
        },
    );

    // 将原本的sigaction拷贝到用户程序指定的地址
    if (retval == Ok(())) && (!old_act.is_null()) {
        if unsafe { !verify_area(old_act as usize as u64, size_of::<sigaction>() as u64) } {
            return SystemError::EFAULT.to_posix_errno() as u64;
        }
        // ！！！！！！！！！！todo: 检查这里old_ka的mask，是否位SIG_IGN SIG_DFL,如果是，则将_sa_handler字段替换为对应的值
        let sah: u64;
        let flag = old_ka.sa_flags & (SA_FLAG_DFL | SA_FLAG_IGN);
        match flag {
            SA_FLAG_DFL => {
                sah = USER_SIG_DFL;
            }
            SA_FLAG_IGN => {
                sah = USER_SIG_IGN;
            }
            _ => sah = unsafe { old_ka._u._sa_handler },
        }
        unsafe {
            (*old_act).sa_handler = sah as *mut c_void;
            (*old_act).sa_flags = old_ka.sa_flags;
            (*old_act).sa_mask = old_ka.sa_mask;
            (*old_act).sa_restorer = old_ka.sa_restorer as *mut c_void;
        }
    }
    //return retval as u64;
    if retval.is_ok(){
        return 0;
    }else{
        return retval.unwrap_err().to_posix_errno() as u64;
    }
    
}

fn do_sigaction(
    sig: SignalNumber,
    act: Option<&mut sigaction>,
    old_act: Option<&mut sigaction>,
) -> Result<(),SystemError> {
    let pcb = current_pcb();

    // 指向当前信号的action的引用
    let action =
        sigaction::convert_mut(unsafe { &mut (*(pcb.sighand)).action[(sig as usize) - 1] })
            .unwrap();

    spin_lock_irq(unsafe { &mut (*(pcb.sighand)).siglock });

    if (action.sa_flags & SA_FLAG_IMMUTABLE) != 0 {
        spin_unlock_irq(unsafe { &mut (*(pcb.sighand)).siglock });
        return Err(SystemError::EINVAL);
    }

    // 如果需要保存原有的sigaction
    // 写的这么恶心，还得感谢rust的所有权系统...old_act的所有权被传入了这个闭包之后，必须要把所有权返回给外面。（也许是我不会用才导致写的这么丑，但是它确实能跑）
    let old_act: Option<&mut sigaction> = {
        if old_act.is_some() {
            let oa = old_act.unwrap();
            *(oa) = *action;
            Some(oa)
        } else {
            None
        }
    };

    // 清除所有的脏的sa_flags位（也就是清除那些未使用的）
    let act = {
        if act.is_some() {
            let ac = act.unwrap();
            ac.sa_flags &= SA_ALL_FLAGS;
            Some(ac)
        } else {
            None
        }
    };

    if old_act.is_some() {
        old_act.unwrap().sa_flags &= SA_ALL_FLAGS;
    }

    if act.is_some() {
        let ac = act.unwrap();
        // 将act.sa_mask的SIGKILL SIGSTOP的屏蔽清除
        sigset_delmask(
            &mut ac.sa_mask,
            sigmask(SignalNumber::SIGKILL) | sigmask(SignalNumber::SIGSTOP),
        );

        // 将新的sigaction拷贝到进程的action中
        *action = *ac;

        /*
        * 根据POSIX 3.3.1.3规定：
        * 1.不管一个信号是否被阻塞，只要将其设置SIG_IGN，如果当前已经存在了正在pending的信号，那么就把这个信号忽略。
        *
        * 2.不管一个信号是否被阻塞，只要将其设置SIG_DFL，如果当前已经存在了正在pending的信号，
              并且对这个信号的默认处理方式是忽略它，那么就会把pending的信号忽略。
        */
        if action.ignored(sig) {
            let mut mask: sigset_t = 0;
            sigset_clear(&mut mask);
            sigset_add(&mut mask, sig);
            let sq: &mut SigQueue = SigQueue::from_c_void(pcb.sig_pending.sigqueue);
            sq.flush_by_mask(&mask);

            // todo: 当有了多个线程后，在这里进行操作，把每个线程的sigqueue都进行刷新
        }
    }

    spin_unlock_irq(unsafe { &mut (*(pcb.sighand)).siglock });
    return Ok(());
}

/// @brief 对于给定的signal number，将u64中对应的位进行置位
pub fn sigmask(sig: SignalNumber) -> u64 {
    // 减1的原因是，sigset的第0位表示信号1
    return 1u64 << ((sig as i32) - 1);
}

#[no_mangle]
pub extern "C" fn sys_rt_sigreturn(regs: &mut pt_regs) -> u64 {
    let frame = regs.rsp as usize as *mut sigframe;

    // 如果当前的rsp不来自用户态，则认为产生了错误（或被SROP攻击）
    if unsafe { !verify_area(frame as u64, size_of::<sigframe>() as u64) } {
        // todo：这里改为生成一个sigsegv
        // 退出进程
        unsafe {
            process_do_exit(SignalNumber::SIGSEGV as u64);
        }
    }

    let mut sigmask: sigset_t = unsafe { (*frame).context.oldmask };
    set_current_sig_blocked(&mut sigmask);

    // 从用户栈恢复sigcontext
    if restore_sigcontext(unsafe { &mut (*frame).context }, regs) == false {
        // todo：这里改为生成一个sigsegv
        // 退出进程
        unsafe {
            process_do_exit(SignalNumber::SIGSEGV as u64);
        }
    }

    // 由于系统调用的返回值会被系统调用模块被存放在rax寄存器，因此，为了还原原来的那个系统调用的返回值，我们需要在这里返回恢复后的rax的值
    return regs.rax;
}

fn set_current_sig_blocked(new_set: &mut sigset_t) {
    sigset_delmask(
        new_set,
        sigmask(SignalNumber::SIGKILL) | sigmask(SignalNumber::SIGSTOP),
    );

    let mut pcb = current_pcb();

    /*
        如果当前pcb的sig_blocked和新的相等，那么就不用改变它。
        请注意，一个进程的sig_blocked字段不能被其他进程修改！
    */
    if sigset_equal(&pcb.sig_blocked, new_set) {
        return;
    }

    let lock: &mut spinlock_t = &mut sighand_struct::convert_mut(pcb.sighand).unwrap().siglock;
    spin_lock_irq(lock);
    // todo: 当一个进程有多个线程后，在这里需要设置每个线程的block字段，并且 retarget_shared_pending（虽然我还没搞明白linux这部分是干啥的）

    // 设置当前进程的sig blocked
    pcb.sig_blocked = *new_set;
    recalc_sigpending();
    spin_unlock_irq(lock);
}
