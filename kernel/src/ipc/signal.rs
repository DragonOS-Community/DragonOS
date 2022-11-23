use core::ptr::read_volatile;

use crate::{
    include::{
        bindings::bindings::{
            pid_t, process_control_block, process_find_pcb_by_pid, pt_regs, spinlock_t, EINVAL,
            ENOTSUP, ESRCH, PF_EXITING, PF_KTHREAD, PF_WAKEKILL, PROC_INTERRUPTIBLE,
        },
        DragonOS::signal::{
            si_code_val, sighand_struct, siginfo, signal_struct, sigpending, sigset_t,
            SignalNumber, MAX_SIG_NUM, sigaction, sigaction__union_u,
        },
    },
    kBUG, kdebug, kwarn,
    libs::{
        ffi_convert::FFIBind2Rust,
        spinlock::{spin_is_locked, spin_lock_irqsave, spin_unlock_irqrestore},
    },
    println,
    process::{
        pid::PidType,
        process::{process_is_stopped, process_kick, process_wake_up_state},
    },
};

use crate::include::DragonOS::signal::{__siginfo_union, __siginfo_union_data};

/// 默认信号处理程序占位符（用于在sighand结构体中的action数组中占位）
pub static DEFAULT_SIGACTION: sigaction = sigaction{
    _u: sigaction__union_u{
        _sa_handler: None,
    },
    sa_flags:0,
    sa_mask:0,
    sa_restorer:None
};

/// @brief kill系统调用，向指定的进程发送信号
/// @param regs->r8 pid 要接收信号的进程id
/// @param regs->r9 sig 信号
#[no_mangle]
pub extern "C" fn sys_kill(regs: &pt_regs) -> u64 {
    println!(
        "sys kill, target pid={}, file={}, line={}",
        regs.r8,
        file!(),
        line!()
    );

    let pid: pid_t = regs.r8 as pid_t;
    let sig: Option<SignalNumber> = SignalNumber::from_i32(regs.r9 as i32);
    if sig.is_none() {
        // 传入的signal数值不合法
        kwarn!("Not a valid signal number");
        return (-(EINVAL as i64)) as u64;
    }

    // 初始化signal info
    let mut info = siginfo {
        _sinfo: __siginfo_union {
            data: __siginfo_union_data {
                si_signo: sig.unwrap() as i32,
                si_code: si_code_val::SI_USER as i32,
                si_errno: 0,
                reserved: 0,
                _sifields: crate::include::DragonOS::signal::__sifields {
                    _kill: crate::include::DragonOS::signal::__sifields__kill { _pid: pid },
                },
            },
        },
    };

    return signal_kill_something_info(sig.unwrap(), Some(&mut info), pid) as u64;
}

/// 通过kill的方式向目标进程发送信号
/// @param sig 要发送的信号
/// @param info 要发送的信息
/// @param pid 进程id（目前只支持pid>0)
fn signal_kill_something_info(sig: SignalNumber, info: Option<&mut siginfo>, pid: pid_t) -> i32 {
    // 暂时不支持特殊的kill操作
    if pid <= 0 {
        kwarn!("Kill operation not support: pid={}", pid);
        return -(ENOTSUP as i32);
    }

    // kill单个进程
    return signal_kill_proc_info(sig, info, pid);
}

fn signal_kill_proc_info(sig: SignalNumber, info: Option<&mut siginfo>, pid: pid_t) -> i32 {
    let mut retval: i32 = -(ESRCH as i32);

    // step1: 当进程管理模块拥有pcblist_lock之后，对其加锁

    // step2: 根据pid找到pcb
    let pcb = unsafe { process_find_pcb_by_pid(pid).as_mut() };

    if pcb.is_none() {
        kwarn!("No such process.");
        return retval;
    }

    println!("Target pcb = {:?}", pcb.as_ref().unwrap());

    // step3: 调用signal_send_sig_info函数，发送信息
    retval = signal_send_sig_info(sig, info, pcb.unwrap());
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
) -> i32 {
    kdebug!("signal_send_sig_info");
    // 检查sig是否符合要求，如果不符合要求，则退出。
    if !verify_signal(sig) {
        return -(EINVAL as i32);
    }

    // 信号符合要求，可以发送

    let mut retval = -(ESRCH as i32);
    let mut flags: u64 = 0;
    // 如果上锁成功，则发送信号
    if !lock_process_sighand(target_pcb, &mut flags).is_none() {
        // 发送信号
        retval = send_signal_locked(sig, info, target_pcb, PidType::PID);
        
        kdebug!("flags=0x{:016x}", flags);
        // 对sighand放锁
        unlock_process_sighand(target_pcb, &flags);
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
    kdebug!("lock_process_sighand");
    let x = unsafe { &mut *pcb.sighand };

    let sighand_ptr = sighand_struct::convert_mut(unsafe { &mut *pcb.sighand });
    // kdebug!("sighand_ptr={:?}", &sighand_ptr);
    if !sighand_ptr.is_some() {
        kBUG!("Sighand ptr of process {pid} is NULL!", pid = pcb.pid);
        return None;
    }else{

        kdebug!("7777");
    }
    let lock =  {&mut sighand_ptr.unwrap().siglock};
    kdebug!("123");
    kdebug!("lock={}", unsafe{*(lock as *mut spinlock_t as *mut i8)});
    spin_lock_irqsave(lock, flags);
    kdebug!("lock={}", unsafe{*(lock as *mut spinlock_t as *mut i8)});
    kdebug!("locked");
    let ret = unsafe { ((*pcb).sighand as *mut sighand_struct).as_mut() };

    return ret;
}

/// @brief 对pcb的sighand结构体中的siglock进行放锁，并恢复之前存储的rflags
/// @param pcb 目标pcb
/// @param flags 用来保存rflags的变量，将这个值恢复到rflags寄存器中
fn unlock_process_sighand(pcb: &mut process_control_block, flags: &u64) {
    kdebug!("unlock_process_sighand");
    let lock = unsafe{&mut (*pcb.sighand).siglock};
    kdebug!("lock={:?}", lock);
    spin_unlock_irqrestore(lock, flags);
    kdebug!("lock={}", unsafe{*(lock as *mut spinlock_t as *mut i8)});
    kdebug!("123443");
}

/// @brief 判断是否需要强制发送信号，然后发送信号
/// 注意，进入该函数前，我们应当对pcb.sighand.siglock加锁。
///
/// @return i32 错误码
fn send_signal_locked(
    sig: SignalNumber,
    info: Option<&mut siginfo>,
    pcb: &mut process_control_block,
    pt: PidType,
) -> i32 {
    kdebug!("send_signal_locked");
    // 是否强制发送信号
    let mut force_send = false;
    // signal的信息为空
    if info.is_none() {
        // todo: 判断signal是否来自于一个祖先进程的namespace，如果是，则强制发送信号
    } else {
        force_send = unsafe { info.as_ref().unwrap()._sinfo.data }.si_code
            == (si_code_val::SI_KERNEL as i32);
    }

    kdebug!("force send={}", force_send);

    return __send_signal_locked(sig, info, pcb, pt, force_send);
}

/// @brief 发送信号
/// 注意，进入该函数前，我们应当对pcb.sighand.siglock加锁。
///
/// @param sig 信号
/// @param _info 信号携带的信息
/// @param pcb 目标进程的pcb
/// @param pt siginfo结构体中，pid字段代表的含义
/// @return i32 错误码
fn __send_signal_locked(
    sig: SignalNumber,
    _info: Option<&mut siginfo>,
    pcb: &mut process_control_block,
    pt: PidType,
    _force_send: bool,
) -> i32 {
    kdebug!("__send_signal_locked");
    let mut retval = 0;

    // 判断该进入该函数时，是否已经持有了锁
    println!("locked={}",spin_is_locked(unsafe { &(*pcb.sighand).siglock }));
    kdebug!("1234");
    let _pending: Option<&mut sigpending> = sigpending::convert_mut(&mut pcb.sig_pending);
    kdebug!("567");

    // 如果是kill或者目标pcb是内核线程，则无需获取sigqueue，直接发送信号即可
    if sig == SignalNumber::SIGKILL || (pcb.flags & (PF_KTHREAD as u64)) != 0 {
        complete_signal(sig, pcb, pt);
    } else {
        // todo: 如果是其他信号，则加入到sigqueue内，然后complete_signal
        retval = -(ENOTSUP as i32);
    }
    kdebug!("12342");
    return retval;
}

/// @brief 将信号添加到目标进程的sig_pending。在引入进程组后，本函数还将负责把信号传递给整个进程组。
///
/// @param sig 信号
/// @param pcb 目标pcb
/// @param pt siginfo结构体中，pid字段代表的含义
fn complete_signal(sig: SignalNumber, pcb: &mut process_control_block, pt: PidType) {
    // todo: 将信号产生的消息通知到正在监听这个信号的进程（引入signalfd之后，在这里调用signalfd_notify)
    kdebug!("complete_signal");
    // 将这个信号加到目标进程的sig_pending中
    sigset_add(
        sigset_t::convert_mut(&mut pcb.sig_pending.signal).unwrap(),
        sig,
    );

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

/// @brief 判断指定的信号在sigset中的对应位是否被置位
/// @return true: 给定的信号在sigset中被置位
/// @return false: 给定的信号在sigset中没有被置位
#[inline]
fn sig_is_member(set: &sigset_t, _sig: SignalNumber) -> bool {
    return if 1 & (set >> ((_sig as u32) - 1)) != 0 {
        true
    } else {
        false
    };
}

/// @brief 将指定的信号在sigset中的对应bit进行置位
#[inline]
fn sigset_add(set: &mut sigset_t, _sig: SignalNumber) {
    *set |= 1 << ((_sig as u32) - 1);
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
    if handler.is_none() {
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
    let mut state: u64 = 0;
    if fatal {
        state = PF_WAKEKILL as u64;
    }
    signal_wake_up_state(pcb, state);
}

fn signal_wake_up_state(pcb: &mut process_control_block, state: u64) {
    assert!(spin_is_locked(&unsafe { *pcb.sighand }.siglock));
    // todo: 设置线程结构体的标志位为TIF_SIGPENDING

    // 如果目标进程已经在运行，则发起一个ipi，使得它陷入内核
    if !process_wake_up_state(pcb, state | (PROC_INTERRUPTIBLE as u64)) {
        process_kick(pcb);
    }
}

