use core::{mem::size_of, sync::atomic::compiler_fence};

use alloc::sync::Arc;

use crate::{
    arch::{
        asm::bitops::ffz,
        interrupt::TrapFrame,
        ipc::signal::{SigCode, SigFlags, SigSet, SignalNumber},
    },
    include::bindings::bindings::{pt_regs, SA_FLAG_DFL, SA_FLAG_IGN},
    ipc::signal_types::SigactionType,
    kdebug, kerror, kwarn,
    libs::spinlock::SpinLockGuard,
    process::{pid::PidType, Pid, ProcessControlBlock, ProcessFlags, ProcessManager, ProcessState},
    syscall::{user_access::UserBufferWriter, SystemError},
};

use super::signal_types::{
    SaHandlerType, SigContext, SigFrame, SigHandStruct, SigInfo, SigPending, SigQueue, SigType,
    Sigaction, SignalStruct, MAX_SIG_NUM,
};

lazy_static! {
    /// 默认信号处理程序占位符（用于在sighand结构体中的action数组中占位）
     #[allow(dead_code)]
    pub static ref DEFAULT_SIGACTION: Sigaction = Sigaction::new(
    SigactionType::SaHandler(SaHandlerType::SigDefault),
     SigFlags::SA_FLAG_DFL,
    SigSet::from_bits(0).unwrap(),
     None,
    );

/// 默认的“忽略信号”的sigaction
#[allow(dead_code)]
pub static ref DEFAULT_SIGACTION_IGNORE: Sigaction = Sigaction::new(
    SigactionType::SaHandler(SaHandlerType::SigDefault),
     SigFlags::SA_FLAG_IGN,
    SigSet::from_bits(0).unwrap(),
     None,
    );

}
/// 通过kill的方式向目标进程发送信号
/// @param sig 要发送的信号
/// @param info 要发送的信息
/// @param pid 进程id（目前只支持pid>0)
pub fn signal_kill_something_info(
    sig: SignalNumber,
    info: Option<&mut SigInfo>,
    pid: Pid,
) -> Result<i32, SystemError> {
    // 暂时不支持特殊的kill操作
    if pid.le(&Pid::from(0)) {
        kwarn!("Kill operation not support: pid={:?}", pid);
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    // kill单个进程
    return signal_kill_proc_info(sig, info, pid);
}

fn signal_kill_proc_info(
    sig: SignalNumber,
    info: Option<&mut SigInfo>,
    pid: Pid,
) -> Result<i32, SystemError> {
    let mut retval = Err(SystemError::ESRCH);

    // step1: 当进程管理模块拥有pcblist_lock之后，对其加锁

    // step2: 根据pid找到pcb
    let pcb = ProcessManager::find(pid);

    if pcb.is_none() {
        kwarn!("No such process.");
        return retval;
    }

    // println!("Target pcb = {:?}", pcb.as_ref().unwrap());
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    // step3: 调用signal_send_sig_info函数，发送信息
    retval = signal_send_sig_info(sig, info, pcb.unwrap(), PidType::PID);
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
    info: Option<&mut SigInfo>,
    target_pcb: Arc<ProcessControlBlock>,
    pt: PidType,
) -> Result<i32, SystemError> {
    // kdebug!("signal_send_sig_info");
    // 检查sig是否符合要求，如果不符合要求，则退出。
    if !verify_signal(sig) {
        return Err(SystemError::EINVAL);
    }
    // 信号符合要求，可以发送
    let mut retval = Err(SystemError::ESRCH);
    let mut flags: usize = 0;
    // 如果上锁成功，则发送信号
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    // 发送信号
    retval = send_signal(sig, info, target_pcb, pt);
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    return retval;
}

/// @brief 判断是否需要强制发送信号，然后发送信号
/// 进入函数后加锁
///
/// @return SystemError 错误码
fn send_signal(
    sig: SignalNumber,
    info: Option<&mut SigInfo>,
    pcb: Arc<ProcessControlBlock>,
    pt: PidType,
) -> Result<i32, SystemError> {
    // 是否强制发送信号
    let mut force_send = false;
    // signal的信息为空

    if let Some(ref x) = info {
        force_send = x.code() == (SigCode::SI_KERNEL as i32);
    } else {
        // todo: 判断signal是否来自于一个祖先进程的namespace，如果是，则强制发送信号
    }

    // kdebug!("force send={}", force_send);

    let _pending = pcb.sig_info_mut().sig_pedding_mut();
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    // 如果是kill或者目标pcb是内核线程，则无需获取sigqueue，直接发送信号即可
    if sig == SignalNumber::SIGKILL || pcb.flags().contains(ProcessFlags::KTHREAD) {
        complete_signal(sig, pcb, pt);
    } else {
        // 如果是其他信号，则加入到sigqueue内，然后complete_signal
        let mut q: SigInfo;
        match info {
            Some(x) => {
                // 已经显式指定了siginfo，则直接使用它。
                q = (*x).clone();
            }
            None => {
                // 不需要显示指定siginfo，因此设置为默认值
                q = SigInfo::new(sig, 0, SigCode::SI_USER, 0, SigType::Kill(Pid::from(0)));
                q.set_sig_type(SigType::Kill(ProcessManager::current_pcb().pid()));
            }
        }

        ProcessManager::current_pcb()
            .sig_info_mut()
            .sig_pedding_mut()
            .queue_mut()
            .q
            .push(q);

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
fn complete_signal(sig: SignalNumber, pcb: Arc<ProcessControlBlock>, pt: PidType) {
    // kdebug!("complete_signal");

    // todo: 将信号产生的消息通知到正在监听这个信号的进程（引入signalfd之后，在这里调用signalfd_notify)
    // 将这个信号加到目标进程的sig_pending中
    pcb.sig_info_mut()
        .sig_pedding_mut()
        .signal_mut()
        .insert(sig.into());
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    // ===== 寻找需要wakeup的目标进程 =====
    // 备注：由于当前没有进程组的概念，每个进程只有1个对应的线程，因此不需要通知进程组内的每个进程。
    //      todo: 当引入进程组的概念后，需要完善这里，使得它能寻找一个目标进程来唤醒，接着执行信号处理的操作。

    // let _signal = pcb.sig_struct();

    let mut _target: Option<Arc<ProcessControlBlock>> = None;

    // 判断目标进程是否想接收这个信号
    if wants_signal(sig, pcb.clone()) {
        _target = Some(pcb.clone());
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
        signal_wake_up(pcb.clone(), sig == SignalNumber::SIGKILL);
    }
}

/// @brief 本函数用于检测指定的进程是否想要接收SIG这个信号。
/// 当我们对于进程组中的所有进程都运行了这个检查之后，我们将可以找到组内愿意接收信号的进程。
/// 这么做是为了防止我们把信号发送给了一个正在或已经退出的进程，或者是不响应该信号的进程。
#[inline]
fn wants_signal(sig: SignalNumber, pcb: Arc<ProcessControlBlock>) -> bool {
    // 如果改进程屏蔽了这个signal，则不能接收
    if pcb.sig_info().sig_block().contains(sig.into()) {
        return false;
    }

    // 如果进程正在退出，则不能接收信号
    if pcb.flags().contains(ProcessFlags::EXITING) {
        return false;
    }

    if sig == SignalNumber::SIGKILL {
        return true;
    }

    if pcb.sched_info().state().is_blocked() {
        return false;
    }

    // todo: 检查目标进程是否正在一个cpu上执行，如果是，则返回true，否则继续检查下一项

    // 检查目标进程是否有信号正在等待处理，如果是，则返回false，否则返回true
    if pcb.sig_info().sig_pedding().signal().bits() == 0 {
        assert!(pcb.sig_info().sig_pedding().queue().q.is_empty());
        return true;
    } else {
        return false;
    }
}

/// @brief 判断signal的处理是否可能使得整个进程组退出
/// @return true 可能会导致退出（不一定）
#[allow(dead_code)]
#[inline]
fn sig_fatal(pcb: Arc<ProcessControlBlock>, sig: SignalNumber) -> bool {
    let action = pcb.sig_struct().handler.0[sig as usize - 1].action();
    // 如果handler是空，采用默认函数，signal处理可能会导致进程退出。
    match action {
        SigactionType::SaHandler(handler) => handler.is_sig_default(),
        SigactionType::SaSigaction(sigaction) => sigaction.is_none(),
    }

    // todo: 参照linux的sig_fatal实现完整功能
}

#[inline]
fn signal_wake_up(pcb: Arc<ProcessControlBlock>, fatal: bool) {
    // kdebug!("signal_wake_up");
    let mut state: ProcessFlags = ProcessFlags::empty();
    if fatal {
        state = ProcessFlags::WAKEKILL;
    }
    signal_wake_up_state(pcb, state);
}

/// 需要保证调用时已经对 sig_struct 上锁
fn signal_wake_up_state(pcb: Arc<ProcessControlBlock>, state: ProcessFlags) {
    // todo: 设置线程结构体的标志位为TIF_SIGPENDING
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    // 如果目标进程已经在运行，则发起一个ipi，使得它陷入内核
    if ProcessManager::wakeup_state(&pcb, state).is_ok() {
        ProcessManager::kick(&pcb);
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

/// @brief 获取要被发送的信号的signumber, SigInfo, 以及对应的sigaction结构体
pub fn get_signal_to_deliver(
    frame: &TrapFrame,
) -> (
    SignalNumber,
    Option<SigInfo>,
    Option<&'static mut Sigaction>,
) {
    let mut info: Option<SigInfo>;
    let ka: Option<&mut Sigaction>;
    let mut sig_number;
    let pcb = ProcessManager::current_pcb();
    let mut guard = pcb.sig_struct_irq();
    let sighand = &mut guard.handler.clone();

    loop {
        (sig_number, info) =
            dequeue_signal(ProcessManager::current_pcb().sig_info_mut().sig_block_mut());

        // 如果信号非法，则直接返回
        if sig_number == SignalNumber::INVALID {
            drop(guard);
            return (sig_number, None, None);
        }
        let tmp_ka: &mut Sigaction;
        // 获取指向sigaction结构体的引用
        let hand = Arc::as_ptr(&guard.handler) as *mut SigHandStruct;
        // kdebug!("hand=0x{:018x}", hand as *const sighand_struct as usize);
        unsafe {
            let r = hand.as_mut();
            if r.is_none() {
                panic!("error converting *mut SigHandStruct to &mut SigHandStruct");
            }
            tmp_ka = &mut r.unwrap().0[sig_number as usize - 1];
        }

        // 如果当前动作是忽略这个信号，则不管它了。
        if tmp_ka.flags().contains(SigFlags::SA_FLAG_IGN) {
            continue;
        } else if !tmp_ka.flags().contains(SigFlags::SA_FLAG_IGN) {
            // 当前不采用默认的信号处理函数
            ka = Some(tmp_ka);
            break;
        }
        kdebug!(
            "Use default handler to handle signal [{}] for pid {:?}",
            sig_number as i32,
            ProcessManager::current_pcb().pid()
        );
        // ===== 经过上面的判断，如果能走到这一步，就意味着我们采用默认的信号处理函数来处理这个信号 =====
        drop(guard);
        // 标记当前进程由于信号而退出
        ProcessManager::current_pcb()
            .flags()
            .insert(ProcessFlags::SIGNALED);

        assert!(info.is_some());
        // 执行进程的退出动作
        ProcessManager::exit(info.unwrap().sig_no() as usize);
        /* NOT REACHED 这部分代码将不会到达 */
    }
    drop(guard);
    return (sig_number, info, ka);
}

/// @brief 从当前进程的sigpending中取出下一个待处理的signal，并返回给调用者。（调用者应当处理这个信号）
/// 请注意，进入本函数前，当前进程应当持有current_pcb().sighand.siglock
fn dequeue_signal(sig_mask: &mut SigSet) -> (SignalNumber, Option<SigInfo>) {
    // kdebug!("dequeue signal");
    // 获取下一个要处理的信号的编号
    let sig = next_signal(
        &ProcessManager::current_pcb().sig_info().sig_pedding(),
        sig_mask,
    );

    let info: Option<SigInfo>;
    if sig != SignalNumber::INVALID {
        // 如果下一个要处理的信号是合法的，则收集其siginfo
        info = Some(collect_signal(
            sig,
            ProcessManager::current_pcb()
                .sig_info_mut()
                .sig_pedding_mut(),
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
fn next_signal(pending: &SigPending, sig_mask: &SigSet) -> SignalNumber {
    let mut sig = SignalNumber::INVALID;

    let s = pending.signal();
    let m = *sig_mask;

    // 获取第一个待处理的信号的号码
    let x = s.intersection(m.complement());
    if x.bits() != 0 {
        sig = SignalNumber::from(ffz(x.complement().bits()) + 1);
        return sig;
    }

    // 暂时只支持64种信号
    assert_eq!(crate::ipc::signal_types::_NSIG_U64_CNT, 1);

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
/// @return SigInfo 信号的信息
fn collect_signal(sig: SignalNumber, pending: &mut SigPending) -> SigInfo {
    let (info, still_pending) = pending.queue_mut().find_and_delete(sig);

    // 如果没有仍在等待的信号，则清除pending位
    if !still_pending {
        pending.signal_mut().remove(sig.into());
    }

    if info.is_some() {
        return info.unwrap();
    } else {
        // 信号不在sigqueue中，这意味着当前信号是来自快速路径，因此直接把siginfo设置为0即可。
        let mut ret = SigInfo::new(sig, 0, SigCode::SI_USER, 0, SigType::Kill(Pid::from(0)));
        ret.set_sig_type(SigType::Kill(Pid::new(0)));
        return ret;
    }
}

/// @brief 刷新指定进程的sighand的sigaction，将满足条件的sigaction恢复为Default
///     除非某个信号被设置为ignore且force_default为false，否则都不会将其恢复
///
/// @param pcb 要被刷新的pcb
/// @param force_default 是否强制将sigaction恢复成默认状态
pub fn flush_signal_handlers(pcb: Arc<ProcessControlBlock>, force_default: bool) {
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    let hand = Arc::as_ptr(&pcb.sig_struct().handler) as *mut SigHandStruct;
    // kdebug!("hand=0x{:018x}", hand as *const sighand_struct as usize);
    let action: &mut [Sigaction; MAX_SIG_NUM as usize];
    unsafe {
        let r = hand.as_mut();
        if r.is_none() {
            panic!("error converting *mut SigHandStruct to &mut SigHandStruct");
        }
        action = &mut r.unwrap().0;
    }

    for ka in action.iter_mut() {
        if force_default || !ka.flags().contains(SigFlags::SA_FLAG_IGN) {
            ka.flags().insert(SigFlags::SA_FLAG_DFL);
            ka.set_action(SigactionType::SaHandler(SaHandlerType::SigDefault));
        }
        // 清除flags中，除了DFL和IGN以外的所有标志

        *ka.flags_mut() &= SigFlags::SA_FLAG_DFL | SigFlags::SA_FLAG_IGN;
        ka.set_restorer(None);
        ka.mask_mut().remove(SigSet::all());
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

pub fn do_sigaction(
    sig: SignalNumber,
    act: Option<&mut Sigaction>,
    old_act: Option<&mut Sigaction>,
) -> Result<(), SystemError> {
    if sig == SignalNumber::INVALID {
        return Err(SystemError::EINVAL);
    }
    let pcb = ProcessManager::current_pcb();
    // 指向当前信号的action的引用
    let action: &mut Sigaction;
    let hand = Arc::as_ptr(&pcb.sig_struct().handler) as *mut SigHandStruct;
    // kdebug!("hand=0x{:018x}", hand as *const sighand_struct as usize);
    unsafe {
        let r = hand.as_mut();
        if r.is_none() {
            panic!("error converting *mut SigHandStruct to &mut SigHandStruct");
        }
        action = &mut r.unwrap().0[sig as usize - 1];
    }

    if action.flags().contains(SigFlags::SA_FLAG_IMMUTABLE) {
        return Err(SystemError::EINVAL);
    }

    // 如果需要保存原有的sigaction
    // 写的这么恶心，还得感谢rust的所有权系统...old_act的所有权被传入了这个闭包之后，必须要把所有权返回给外面。（也许是我不会用才导致写的这么丑，但是它确实能跑）
    let old_act: Option<&mut Sigaction> = {
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
            *ac.flags_mut() &= SigFlags::SA_FLAG_ALL;
            Some(ac)
        } else {
            None
        }
    };

    if old_act.is_some() {
        *old_act.unwrap().flags_mut() &= SigFlags::SA_FLAG_ALL;
    }

    if act.is_some() {
        let ac = act.unwrap();
        // 将act.sa_mask的SIGKILL SIGSTOP的屏蔽清除
        ac.mask_mut().remove(
            SigSet::from(SignalNumber::SIGKILL.into()) | SigSet::from(SignalNumber::SIGSTOP.into()),
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
        if action.ignore(sig) {
            let mut mask: SigSet = SigSet::from_bits_truncate(0);
            mask.insert(sig.into());
            pcb.sig_info_mut()
                .sig_pedding_mut()
                .queue_mut()
                .flush_by_mask(&mask);

            // todo: 当有了多个线程后，在这里进行操作，把每个线程的sigqueue都进行刷新
        }
    }

    return Ok(());
}

pub fn set_current_sig_blocked(new_set: &mut SigSet) {
    new_set.remove(
        SigSet::from(SignalNumber::SIGKILL.into()) | SigSet::from(SignalNumber::SIGSTOP.into()),
    );

    let pcb = ProcessManager::current_pcb();

    /*
        如果当前pcb的sig_blocked和新的相等，那么就不用改变它。
        请注意，一个进程的sig_blocked字段不能被其他进程修改！
    */
    if pcb.sig_info().sig_block().eq(new_set) {
        return;
    }

    let guard = pcb.sig_struct_irq();
    // todo: 当一个进程有多个线程后，在这里需要设置每个线程的block字段，并且 retarget_shared_pending（虽然我还没搞明白linux这部分是干啥的）

    // 设置当前进程的sig blocked
    *pcb.sig_info_mut().sig_block_mut() = *new_set;
    recalc_sigpending();
    drop(guard);
}
