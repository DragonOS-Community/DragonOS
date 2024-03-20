use core::sync::atomic::compiler_fence;

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::ipc::signal::{SigCode, SigFlags, SigSet, Signal},
    ipc::signal_types::SigactionType,
    kwarn,
    libs::spinlock::SpinLockGuard,
    process::{pid::PidType, Pid, ProcessControlBlock, ProcessFlags, ProcessManager},
};

use super::signal_types::{
    SaHandlerType, SigInfo, SigType, Sigaction, SignalStruct, SIG_KERNEL_STOP_MASK,
};

impl Signal {
    /// 向目标进程发送信号
    ///
    /// ## 参数
    ///
    /// - `sig` 要发送的信号
    /// - `info` 要发送的信息
    /// -  `pid` 进程id（目前只支持pid>0)
    pub fn send_signal_info(
        &self,
        info: Option<&mut SigInfo>,
        pid: Pid,
    ) -> Result<i32, SystemError> {
        // TODO:暂时不支持特殊的信号操作，待引入进程组后补充
        // 如果 pid 大于 0，那么会发送信号给 pid 指定的进程
        // 如果 pid 等于 0，那么会发送信号给与调用进程同组的每个进程，包括调用进程自身
        // 如果 pid 小于 -1，那么会向组 ID 等于该 pid 绝对值的进程组内所有下属进程发送信号。向一个进程组的所有进程发送信号在 shell 作业控制中有特殊有途
        // 如果 pid 等于 -1，那么信号的发送范围是：调用进程有权将信号发往的每个目标进程，除去 init（进程 ID 为 1）和调用进程自身。如果特权级进程发起这一调用，那么会发送信号给系统中的所有进程，上述两个进程除外。显而易见，有时也将这种信号发送方式称之为广播信号
        // 如果并无进程与指定的 pid 相匹配，那么 kill() 调用失败，同时将 errno 置为 ESRCH（“查无此进程”）
        if pid.lt(&Pid::from(0)) {
            kwarn!("Kill operation not support: pid={:?}", pid);
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        // 检查sig是否符合要求，如果不符合要求，则退出。
        if !self.is_valid() {
            return Err(SystemError::EINVAL);
        }
        let mut retval = Err(SystemError::ESRCH);
        let pcb = ProcessManager::find(pid);

        if pcb.is_none() {
            kwarn!("No such process.");
            return retval;
        }

        let pcb = pcb.unwrap();
        // println!("Target pcb = {:?}", pcb.as_ref().unwrap());
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        // 发送信号
        retval = self.send_signal(info, pcb.clone(), PidType::PID);

        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        return retval;
    }

    /// @brief 判断是否需要强制发送信号，然后发送信号
    /// 进入函数后加锁
    ///
    /// @return SystemError 错误码
    fn send_signal(
        &self,
        info: Option<&mut SigInfo>,
        pcb: Arc<ProcessControlBlock>,
        pt: PidType,
    ) -> Result<i32, SystemError> {
        // 是否强制发送信号
        let mut force_send = false;
        // signal的信息为空

        if let Some(ref siginfo) = info {
            force_send = matches!(siginfo.sig_code(), SigCode::Kernel);
        } else {
            // todo: 判断signal是否来自于一个祖先进程的namespace，如果是，则强制发送信号
            //详见 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/signal.c?r=&mo=32170&fi=1220#1226
        }

        if !self.prepare_sianal(pcb.clone(), force_send) {
            return Err(SystemError::EINVAL);
        }
        // kdebug!("force send={}", force_send);
        let pcb_info = pcb.sig_info_irqsave();
        let pending = if matches!(pt, PidType::PID) {
            pcb_info.sig_shared_pending()
        } else {
            pcb_info.sig_pending()
        };
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        // 如果是kill或者目标pcb是内核线程，则无需获取sigqueue，直接发送信号即可
        if matches!(self, Signal::SIGKILL) || pcb.flags().contains(ProcessFlags::KTHREAD) {
            //避免死锁
            drop(pcb_info);
            self.complete_signal(pcb.clone(), pt);
        }
        // 如果不是实时信号的话，同一时刻信号队列里只会有一个待处理的信号，如果重复接收就不做处理
        else if !self.is_rt_signal() && pending.queue().find(self.clone()).0.is_some() {
            return Ok(0);
        } else {
            // TODO signalfd_notify 完善 signalfd 机制
            // 如果是其他信号，则加入到sigqueue内，然后complete_signal
            let new_sig_info = match info {
                Some(siginfo) => {
                    // 已经显式指定了siginfo，则直接使用它。
                    (*siginfo).clone()
                }
                None => {
                    // 不需要显示指定siginfo，因此设置为默认值
                    SigInfo::new(
                        self.clone(),
                        0,
                        SigCode::User,
                        SigType::Kill(ProcessManager::current_pcb().pid()),
                    )
                }
            };
            drop(pcb_info);
            pcb.sig_info_mut()
                .sig_pending_mut()
                .queue_mut()
                .q
                .push(new_sig_info);

            if pt == PidType::PGID || pt == PidType::SID {}
            self.complete_signal(pcb.clone(), pt);
        }
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        return Ok(0);
    }

    /// @brief 将信号添加到目标进程的sig_pending。在引入进程组后，本函数还将负责把信号传递给整个进程组。
    ///
    /// @param sig 信号
    /// @param pcb 目标pcb
    /// @param pt siginfo结构体中，pid字段代表的含义
    fn complete_signal(&self, pcb: Arc<ProcessControlBlock>, pt: PidType) {
        // kdebug!("complete_signal");

        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        // ===== 寻找需要wakeup的目标进程 =====
        // 备注：由于当前没有进程组的概念，每个进程只有1个对应的线程，因此不需要通知进程组内的每个进程。
        //      todo: 当引入进程组的概念后，需要完善这里，使得它能寻找一个目标进程来唤醒，接着执行信号处理的操作。

        // let _signal = pcb.sig_struct();

        let target_pcb: Option<Arc<ProcessControlBlock>>;

        // 判断目标进程是否想接收这个信号
        if self.wants_signal(pcb.clone()) {
            // todo: 将信号产生的消息通知到正在监听这个信号的进程（引入signalfd之后，在这里调用signalfd_notify)
            // 将这个信号加到目标进程的sig_pending中
            pcb.sig_info_mut()
                .sig_pending_mut()
                .signal_mut()
                .insert(self.clone().into());
            target_pcb = Some(pcb.clone());
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

        // TODO:引入进程组后，在这里挑选一个进程来唤醒，让它执行相应的操作。
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        // TODO: 到这里，信号已经被放置在共享的pending队列中，我们在这里把目标进程唤醒。
        if let Some(target_pcb) = target_pcb {
            let guard = target_pcb.sig_struct();
            signal_wake_up(target_pcb.clone(), guard, *self == Signal::SIGKILL);
        }
    }

    /// @brief 本函数用于检测指定的进程是否想要接收SIG这个信号。
    /// 当我们对于进程组中的所有进程都运行了这个检查之后，我们将可以找到组内愿意接收信号的进程。
    /// 这么做是为了防止我们把信号发送给了一个正在或已经退出的进程，或者是不响应该信号的进程。
    #[inline]
    fn wants_signal(&self, pcb: Arc<ProcessControlBlock>) -> bool {
        // 如果改进程屏蔽了这个signal，则不能接收
        if pcb
            .sig_info_irqsave()
            .sig_block()
            .contains(self.clone().into())
        {
            return false;
        }

        // 如果进程正在退出，则不能接收信号
        if pcb.flags().contains(ProcessFlags::EXITING) {
            return false;
        }

        if *self == Signal::SIGKILL {
            return true;
        }
        let state = pcb.sched_info().inner_lock_read_irqsave().state();
        if state.is_blocked() && (state.is_blocked_interruptable() == false) {
            return false;
        }

        // todo: 检查目标进程是否正在一个cpu上执行，如果是，则返回true，否则继续检查下一项

        // 检查目标进程是否有信号正在等待处理，如果是，则返回false，否则返回true
        if pcb.sig_info_irqsave().sig_pending().signal().bits() == 0 {
            return true;
        } else {
            return false;
        }
    }

    /// @brief 判断signal的处理是否可能使得整个进程组退出
    /// @return true 可能会导致退出（不一定）
    #[allow(dead_code)]
    #[inline]
    fn sig_fatal(&self, pcb: Arc<ProcessControlBlock>) -> bool {
        let action = pcb.sig_struct().handlers[self.clone() as usize - 1].action();
        // 如果handler是空，采用默认函数，signal处理可能会导致进程退出。
        match action {
            SigactionType::SaHandler(handler) => handler.is_sig_default(),
            SigactionType::SaSigaction(sigaction) => sigaction.is_none(),
        }
        // todo: 参照linux的sig_fatal实现完整功能
    }

    /// 检查信号是否能被发送，并且而且要处理 SIGCONT 和 STOP 信号
    ///
    /// ## 参数
    ///
    /// - `pcb` 要发送信号的目标pcb
    ///
    /// - `force` 是否强制发送(指走 fast path ， 不加入 sigpending按顺序处理，直接进入 complete_signal)
    ///
    /// ## 返回值
    ///
    /// - `true` 能够发送信号
    ///
    /// - `false` 不能发送信号
    fn prepare_sianal(&self, pcb: Arc<ProcessControlBlock>, _force: bool) -> bool {
        let flush: SigSet;
        if !(self.into_sigset() & SIG_KERNEL_STOP_MASK).is_empty() {
            flush = Signal::SIGCONT.into_sigset();
            pcb.sig_info_mut()
                .sig_shared_pending_mut()
                .flush_by_mask(&flush);
            // TODO 对每个子线程 flush mask
        } else if *self == Signal::SIGCONT {
            flush = SIG_KERNEL_STOP_MASK;
            assert!(!flush.is_empty());
            pcb.sig_info_mut()
                .sig_shared_pending_mut()
                .flush_by_mask(&flush);
            let _r = ProcessManager::wakeup_stop(&pcb);
            // TODO 对每个子线程 flush mask
            // 这里需要补充一段逻辑，详见https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/signal.c#952
        }

        // 一个被阻塞了的信号肯定是要被处理的
        if pcb
            .sig_info_irqsave()
            .sig_block()
            .contains(self.into_sigset())
        {
            return true;
        }
        return !pcb.sig_struct().handlers[self.clone() as usize - 1].is_ignore();

        //TODO 仿照 linux 中的prepare signal完善逻辑，linux 中还会根据例如当前进程状态(Existing)进行判断，现在的信号能否发出就只是根据 ignored 来判断
    }
}

/// 因收到信号而唤醒进程
///
/// ## 参数
///
/// - `pcb` 要唤醒的进程pcb
/// - `_guard` 信号结构体锁守卫，来保证信号结构体已上锁
/// - `fatal` 表明这个信号是不是致命的(会导致进程退出)
#[inline]
fn signal_wake_up(pcb: Arc<ProcessControlBlock>, _guard: SpinLockGuard<SignalStruct>, fatal: bool) {
    // 如果是 fatal 的话就唤醒 stop 和 block 的进程来响应，因为唤醒后就会终止
    // 如果不是 fatal 的就只唤醒 stop 的进程来响应
    // kdebug!("signal_wake_up");
    // 如果目标进程已经在运行，则发起一个ipi，使得它陷入内核
    let state = pcb.sched_info().inner_lock_read_irqsave().state();
    let mut wakeup_ok = true;
    if state.is_blocked_interruptable() {
        ProcessManager::wakeup(&pcb).unwrap_or_else(|e| {
            wakeup_ok = false;
            kwarn!(
                "Current pid: {:?}, signal_wake_up target {:?} error: {:?}",
                ProcessManager::current_pcb().pid(),
                pcb.pid(),
                e
            );
        });
    } else if state.is_stopped() {
        ProcessManager::wakeup_stop(&pcb).unwrap_or_else(|e| {
            wakeup_ok = false;
            kwarn!(
                "Current pid: {:?}, signal_wake_up target {:?} error: {:?}",
                ProcessManager::current_pcb().pid(),
                pcb.pid(),
                e
            );
        });
    } else {
        wakeup_ok = false;
    }

    if wakeup_ok {
        ProcessManager::kick(&pcb);
    } else {
        if fatal {
            let _r = ProcessManager::wakeup(&pcb).map(|_| {
                ProcessManager::kick(&pcb);
            });
        }
    }
}

/// @brief 当一个进程具有多个线程之后，在这里需要重新计算线程的flag中的TIF_SIGPENDING位
fn recalc_sigpending() {
    // todo:
}

/// @brief 刷新指定进程的sighand的sigaction，将满足条件的sigaction恢复为Default
///     除非某个信号被设置为ignore且force_default为false，否则都不会将其恢复
///
/// @param pcb 要被刷新的pcb
/// @param force_default 是否强制将sigaction恢复成默认状态
pub fn flush_signal_handlers(pcb: Arc<ProcessControlBlock>, force_default: bool) {
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    // kdebug!("hand=0x{:018x}", hand as *const sighand_struct as usize);
    let actions = &mut pcb.sig_struct_irqsave().handlers;

    for sigaction in actions.iter_mut() {
        if force_default || !sigaction.is_ignore() {
            sigaction.set_action(SigactionType::SaHandler(SaHandlerType::SigDefault));
        }
        // 清除flags中，除了DFL和IGN以外的所有标志
        sigaction.set_restorer(None);
        sigaction.mask_mut().remove(SigSet::all());
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

pub(super) fn do_sigaction(
    sig: Signal,
    act: Option<&mut Sigaction>,
    old_act: Option<&mut Sigaction>,
) -> Result<(), SystemError> {
    if sig == Signal::INVALID {
        return Err(SystemError::EINVAL);
    }
    let pcb = ProcessManager::current_pcb();
    // 指向当前信号的action的引用
    let action: &mut Sigaction = &mut pcb.sig_struct().handlers[sig as usize - 1];

    // 对比 MUSL 和 relibc ， 暂时不设置这个标志位
    // if action.flags().contains(SigFlags::SA_FLAG_IMMUTABLE) {
    //     return Err(SystemError::EINVAL);
    // }

    // 保存原有的 sigaction
    let old_act: Option<&mut Sigaction> = {
        if old_act.is_some() {
            let oa = old_act.unwrap();
            *(oa) = (*action).clone();
            Some(oa)
        } else {
            None
        }
    };
    // 清除所有的脏的sa_flags位（也就是清除那些未使用的）
    let act = {
        if act.is_some() {
            let ac = act.unwrap();
            *ac.flags_mut() &= SigFlags::SA_ALL;
            Some(ac)
        } else {
            None
        }
    };

    if old_act.is_some() {
        *old_act.unwrap().flags_mut() &= SigFlags::SA_ALL;
    }

    if act.is_some() {
        let ac = act.unwrap();
        // 将act.sa_mask的SIGKILL SIGSTOP的屏蔽清除
        ac.mask_mut()
            .remove(SigSet::from(Signal::SIGKILL.into()) | SigSet::from(Signal::SIGSTOP.into()));

        // 将新的sigaction拷贝到进程的action中
        *action = *ac;
        /*
        * 根据POSIX 3.3.1.3规定：
        * 1.不管一个信号是否被阻塞，只要将其设置SIG_IGN，如果当前已经存在了正在pending的信号，那么就把这个信号忽略。
        *
        * 2.不管一个信号是否被阻塞，只要将其设置SIG_DFL，如果当前已经存在了正在pending的信号，
              并且对这个信号的默认处理方式是忽略它，那么就会把pending的信号忽略。
        */
        if action.is_ignore() {
            let mut mask: SigSet = SigSet::from_bits_truncate(0);
            mask.insert(sig.into());
            pcb.sig_info_mut().sig_pending_mut().flush_by_mask(&mask);
            // todo: 当有了多个线程后，在这里进行操作，把每个线程的sigqueue都进行刷新
        }
    }
    return Ok(());
}

/// 设置当前进程的屏蔽信号 (sig_block)，待引入 [sigprocmask](https://man7.org/linux/man-pages/man2/sigprocmask.2.html) 系统调用后要删除这个散装函数
///
/// ## 参数
///
/// - `new_set` 新的屏蔽信号bitmap的值
pub fn set_current_sig_blocked(new_set: &mut SigSet) {
    new_set.remove(SigSet::from(Signal::SIGKILL.into()) | SigSet::from(Signal::SIGSTOP.into()));
    //TODO 把这个散装函数用 sigsetops 替换掉
    let pcb = ProcessManager::current_pcb();

    /*
        如果当前pcb的sig_blocked和新的相等，那么就不用改变它。
        请注意，一个进程的sig_blocked字段不能被其他进程修改！
    */
    if pcb.sig_info_irqsave().sig_block().eq(new_set) {
        return;
    }

    let guard = pcb.sig_struct_irqsave();
    // todo: 当一个进程有多个线程后，在这里需要设置每个线程的block字段，并且 retarget_shared_pending（虽然我还没搞明白linux这部分是干啥的）

    // 设置当前进程的sig blocked
    *pcb.sig_info_mut().sig_block_mut() = *new_set;
    recalc_sigpending();
    drop(guard);
}
