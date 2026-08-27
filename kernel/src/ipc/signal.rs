use core::{
    fmt::Debug,
    intrinsics::unlikely,
    sync::atomic::{compiler_fence, Ordering},
};

use alloc::sync::Arc;
use log::warn;
use system_error::SystemError;

use crate::{
    arch::ipc::signal::{SigSet, Signal},
    ipc::signal_types::{
        SaHandlerType, SigCode, SigInfo, SigType, SigactionType, SignalFlags,
        SIG_KERNEL_COREDUMP_MASK, SIG_KERNEL_IGNORE_MASK, SIG_KERNEL_ONLY_MASK,
        SIG_KERNEL_STOP_MASK,
    },
    libs::rwlock::RwLockWriteGuard,
    mm::VirtAddr,
    process::{
        pid::PidType, ProcessControlBlock, ProcessFlags, ProcessManager, ProcessSignalInfo, RawPid,
    },
    syscall::user_access::UserBufferWriter,
    time::{
        sleep::nanosleep,
        syscall::{posix_clock_now, PosixClockID},
        Instant, PosixTimeSpec,
    },
};

/// Send a kernel-originated signal to the current task.
///
/// This is a small convenience wrapper to avoid duplicating SigInfo construction
/// (and pid/uid filling) in subsystems like pipe.
pub fn send_kernel_signal_to_current(sig: Signal) -> Result<(), SystemError> {
    let pid = ProcessManager::current_pcb().task_pid_vnr();

    // For kernel-originated signals, use SigCode::Kernel and a zeroed sender.
    // This keeps the responsibility of siginfo formatting inside the signal subsystem.
    let mut info = SigInfo::new(
        sig,
        0,
        SigCode::Kernel,
        SigType::Kill {
            pid: RawPid::new(0),
            uid: 0,
        },
    );

    compiler_fence(Ordering::SeqCst);
    let ret = sig.send_signal_info(Some(&mut info), pid);
    compiler_fence(Ordering::SeqCst);
    ret.map(|_| ())
}

/// Force a kernel-originated signal to the current thread.
///
/// This mirrors Linux's force-signal behavior used by rseq error handling:
/// a blocked or ignored fatal signal must be made deliverable instead of being
/// left pending forever.
pub fn force_kernel_signal_to_current(sig: Signal) -> Result<(), SystemError> {
    let pcb = ProcessManager::current_pcb();

    if let Some(mut action) = pcb.sighand().handler(sig) {
        let blocked = pcb
            .sig_info_irqsave()
            .sig_blocked()
            .contains(sig.into_sigset());
        if blocked || action.is_ignore() {
            action.set_action(SigactionType::SaHandler(SaHandlerType::Default));
            pcb.sighand().set_handler(sig, action);
        }

        if action.is_default() {
            pcb.sighand().flags_remove(SignalFlags::UNKILLABLE);
        }
    }

    {
        let mut siginfo = pcb.sig_info_mut();
        siginfo.sig_block_mut().remove(sig.into_sigset());
        siginfo.saved_sigmask_mut().remove(sig.into_sigset());
    }
    pcb.recalc_sigpending();

    let mut info = SigInfo::new(
        sig,
        0,
        SigCode::Kernel,
        SigType::Kill {
            pid: RawPid::new(0),
            uid: 0,
        },
    );

    compiler_fence(Ordering::SeqCst);
    let ret = sig.send_signal_info_to_pcb(Some(&mut info), pcb, PidType::PID);
    compiler_fence(Ordering::SeqCst);
    ret.map(|_| ())
}

/// Force a synchronous fault signal to the current thread.
///
/// This mirrors Linux `force_sig_fault(sig, code, addr)`: the signal targets
/// `current` directly and carries `si_code` plus `si_addr` for SA_SIGINFO.
pub fn force_sig_fault_to_current(
    sig: Signal,
    code: i32,
    addr: VirtAddr,
) -> Result<(), SystemError> {
    let pcb = ProcessManager::current_pcb();

    if let Some(mut action) = pcb.sighand().handler(sig) {
        let blocked = pcb
            .sig_info_irqsave()
            .sig_blocked()
            .contains(sig.into_sigset());
        if blocked || action.is_ignore() {
            action.set_action(SigactionType::SaHandler(SaHandlerType::Default));
            pcb.sighand().set_handler(sig, action);
        }

        if action.is_default() {
            pcb.sighand().flags_remove(SignalFlags::UNKILLABLE);
        }
    }

    {
        let mut siginfo = pcb.sig_info_mut();
        siginfo.sig_block_mut().remove(sig.into_sigset());
        siginfo.saved_sigmask_mut().remove(sig.into_sigset());
    }
    pcb.recalc_sigpending();

    let mut info = SigInfo::new(
        sig,
        0,
        SigCode::Raw(code),
        SigType::Fault {
            addr: addr.data() as u64,
            addr_lsb: 0,
        },
    );

    compiler_fence(Ordering::SeqCst);
    let ret = sig.send_signal_info_to_pcb(Some(&mut info), pcb, PidType::PID);
    compiler_fence(Ordering::SeqCst);
    ret.map(|_| ())
}

/// Force a kernel-originated signal to the current thread with its default
/// disposition, even if userspace installed a handler.
pub fn force_kernel_default_signal_to_current(sig: Signal) -> Result<(), SystemError> {
    let pcb = ProcessManager::current_pcb();

    if let Some(mut action) = pcb.sighand().handler(sig) {
        action.set_action(SigactionType::SaHandler(SaHandlerType::Default));
        pcb.sighand().set_handler(sig, action);
    }
    pcb.sighand().flags_remove(SignalFlags::UNKILLABLE);

    {
        let mut siginfo = pcb.sig_info_mut();
        siginfo.sig_block_mut().remove(sig.into_sigset());
        siginfo.saved_sigmask_mut().remove(sig.into_sigset());
    }
    pcb.recalc_sigpending();

    let mut info = SigInfo::new(
        sig,
        0,
        SigCode::Kernel,
        SigType::Kill {
            pid: RawPid::new(0),
            uid: 0,
        },
    );

    compiler_fence(Ordering::SeqCst);
    let ret = sig.send_signal_info_to_pcb(Some(&mut info), pcb, PidType::PID);
    compiler_fence(Ordering::SeqCst);
    ret.map(|_| ())
}

impl Signal {
    pub fn signal_pending_state(
        interruptible: bool,
        task_wake_kill: bool,
        pcb: &Arc<ProcessControlBlock>,
    ) -> bool {
        if !interruptible && !task_wake_kill {
            return false;
        }

        // Linux ptrace_signal_wake_up() publishes TIF_SIGPENDING even though
        // PTRACE_INTERRUPT does not enqueue a signal. Treat DragonOS's
        // equivalent return-to-user work bit the same way for interruptible
        // sleeps, so a tracee cannot race the wakeup and block after the
        // tracer has requested an EVENT_STOP. Killable/uninterruptible waits
        // remain wakeable only by a fatal signal.
        if interruptible && pcb.flags().contains(ProcessFlags::PENDING_PTRACE_STOP) {
            return true;
        }

        if !pcb.has_pending_signal_fast() {
            return false;
        }

        return interruptible || Self::fatal_signal_pending(pcb);
    }

    /// 判断当前进程是否收到了SIGKILL信号
    pub fn fatal_signal_pending(pcb: &Arc<ProcessControlBlock>) -> bool {
        let guard = pcb.sig_info_irqsave();
        if guard
            .sig_pending()
            .signal()
            .contains(Signal::SIGKILL.into())
        {
            return true;
        }

        return false;
    }

    /// Fatal SIGKILL check for the OOM path.
    ///
    /// DragonOS currently stores process-level signals in `sighand.shared_pending`,
    /// while the global `fatal_signal_pending()` only checks thread-level pending.
    /// The OOM killer needs to know whether the current task is already destined to
    /// exit, to avoid continuing to select and kill new victims. This helper is
    /// read-only and does not consume the signal.
    pub fn oom_fatal_signal_pending(pcb: &Arc<ProcessControlBlock>) -> bool {
        if pcb.sighand().group_exit_code_if_set().is_some() {
            return true;
        }

        if pcb
            .sighand()
            .shared_pending_signal()
            .contains(Signal::SIGKILL.into())
        {
            return true;
        }

        Self::fatal_signal_pending(pcb)
    }

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
        pid: RawPid,
    ) -> Result<i32, SystemError> {
        // TODO:暂时不支持特殊的信号操作，待引入进程组后补充
        // 如果 pid 大于 0，那么会发送信号给 pid 指定的进程
        // 如果 pid 等于 0，那么会发送信号给与调用进程同组的每个进程，包括调用进程自身
        // 如果 pid 小于 -1，那么会向组 ID 等于该 pid 绝对值的进程组内所有下属进程发送信号。向一个进程组的所有进程发送信号在 shell 作业控制中有特殊有途
        // 如果 pid 等于 -1，那么信号的发送范围是：调用进程有权将信号发往的每个目标进程，除去 init（进程 ID 为 1）和调用进程自身。如果特权级进程发起这一调用，那么会发送信号给系统中的所有进程，上述两个进程除外。显而易见，有时也将这种信号发送方式称之为广播信号
        // 如果并无进程与指定的 pid 相匹配，那么 kill() 调用失败，同时将 errno 置为 ESRCH（“查无此进程”）
        if pid.lt(&RawPid::from(0)) {
            warn!("Kill operation not support: pid={:?}", pid);
            return Err(SystemError::ENOSYS);
        }

        // 暂时不支持发送信号给进程组
        if pid.data() == 0 {
            return Err(SystemError::ENOSYS);
        }
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        // 检查sig是否符合要求，如果不符合要求，则退出。
        if !self.is_valid() {
            return Err(SystemError::EINVAL);
        }
        let retval = Err(SystemError::ESRCH);
        let pcb = ProcessManager::find_task_by_vpid(pid);

        if pcb.is_none() {
            warn!("No such process: pid={:?}", pid);
            return retval;
        }

        let pcb = pcb.unwrap();
        return self.send_signal_info_to_pcb(info, pcb, PidType::TGID);
    }

    /// 直接向指定进程发送信号，绕过PID namespace查找
    ///
    /// # 参数
    /// - `info`: 信号信息
    /// - `pcb`: 目标进程
    /// - `pt`: 信号类型，`PidType::PID` 表示线程级信号，`PidType::TGID` 表示进程级信号
    pub fn send_signal_info_to_pcb(
        &self,
        info: Option<&mut SigInfo>,
        pcb: Arc<ProcessControlBlock>,
        pt: PidType,
    ) -> Result<i32, SystemError> {
        // 检查sig是否符合要求，如果不符合要求，则退出。
        if !self.is_valid() {
            return Err(SystemError::EINVAL);
        }
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        // 发送信号
        let retval = self.send_signal(info, pcb, pt);
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        return retval;
    }

    /// @brief 判断是否需要强制发送信号，然后发送信号
    /// 进入函数后加锁
    ///
    /// @return SystemError 错误码
    fn send_signal(
        &self,
        mut info: Option<&mut SigInfo>,
        pcb: Arc<ProcessControlBlock>,
        pt: PidType,
    ) -> Result<i32, SystemError> {
        let force_send = should_force_send(info.as_deref_mut(), &pcb);

        let prepare_result = self.prepare_sianal(pcb.clone(), force_send);
        if !prepare_result {
            return Ok(0);
        }
        // debug!("force send={}", force_send);
        // 根据 Linux 语义：PidType::PID 表示线程级信号，其他类型（TGID/PGID/SID）表示进程级信号
        let is_thread_target = matches!(pt, PidType::PID);

        // SIGKILL and kernel threads take the fast path: skip enqueueing siginfo and directly set the pending bitmap and wake up
        if matches!(self, Signal::SIGKILL) || pcb.flags().contains(ProcessFlags::KTHREAD) {
            self.complete_signal(pcb.clone(), pt, false);
            return Ok(0);
        }

        let new_sig_info = match info {
            Some(siginfo) => {
                // An explicit siginfo was provided, so use it directly.
                *siginfo
            }
            None => {
                // No explicit siginfo was provided, so fill in the default values
                let current_pcb = ProcessManager::current_pcb();
                let sender_pid = current_pcb.raw_pid();
                let sender_uid = current_pcb.cred().uid.data() as u32;
                SigInfo::new(
                    *self,
                    0,
                    SigCode::User,
                    SigType::Kill {
                        pid: sender_pid,
                        uid: sender_uid,
                    },
                )
            }
        };

        if is_thread_target {
            // Thread-level signal: add to the thread's sig_pending
            let mut pcb_info = pcb.sig_info_mut();
            if !self.is_rt_signal() && pcb_info.sig_pending().queue().find(*self).0.is_some() {
                return Ok(0);
            }
            pcb_info.sig_pending_mut().queue_mut().q.push(new_sig_info);
        } else {
            // Process-level signal: add to shared_pending
            if !pcb.sighand().shared_pending_push_dedup(*self, new_sig_info) {
                return Ok(0);
            }
        }

        // On the normal path the shared bitmap has already been set when the signal was enqueued.
        self.complete_signal(pcb.clone(), pt, true);
        Ok(0)
    }

    /// 在已持有 ProcessSignalInfo 锁的情况下，将信号入队
    ///
    /// 此方法专为 POSIX timer 等需要原子性检查并入队的场景设计，
    /// 调用者负责在调用前检查去重条件，此方法只负责入队和后续处理。
    ///
    /// ## 参数
    /// - `info`: 要入队的信号信息
    /// - `pcb`: 目标进程
    /// - `pt`: 信号类型，`PidType::PID` 表示线程级信号，`PidType::TGID` 表示进程级信号
    /// - `siginfo_guard`: 已持有的 ProcessSignalInfo 锁（线程级信号时使用，进程级信号时会被忽略）
    ///
    /// ## 注意
    /// 此方法会消耗 `siginfo_guard`，调用后锁会被释放
    pub fn enqueue_signal_locked(
        &self,
        info: SigInfo,
        pcb: Arc<ProcessControlBlock>,
        pt: PidType,
        siginfo_guard: RwLockWriteGuard<'_, ProcessSignalInfo>,
    ) {
        let is_thread_target = matches!(pt, PidType::PID);

        // 根据信号类型选择添加到线程级 pending 还是进程级 shared_pending
        if is_thread_target {
            // 线程级信号：添加到线程的 sig_pending
            let mut guard = siginfo_guard;
            guard.sig_pending_mut().queue_mut().q.push(info);
            drop(guard);
        } else {
            // 进程级信号：添加到 shared_pending（不需要 siginfo_guard）
            drop(siginfo_guard);
            pcb.sighand().shared_pending_push(*self, info);
        }

        // complete_signal 会统一：设置对应 pending 位图、更新 HAS_PENDING_SIGNAL，并按需唤醒
        self.complete_signal(pcb, pt, true);
    }

    /// @brief Complete signal delivery: set the pending bitmap, choose the delivery target, and wake up as needed.
    /// @param shared_bitmap_set Whether the shared bitmap for process-level signals was already set at enqueue time
    /// (true on the normal enqueue path; false on the SIGKILL/KTHREAD fast path, where this function sets it)
    #[allow(clippy::if_same_then_else)]
    pub(crate) fn complete_signal(
        &self,
        pcb: Arc<ProcessControlBlock>,
        pt: PidType,
        shared_bitmap_set: bool,
    ) {
        // 根据信号类型选择添加到线程级 pending 还是进程级 shared_pending
        let is_thread_target = matches!(pt, PidType::PID);
        let target_pcb = if is_thread_target {
            if self.wants_signal(pcb.clone()) {
                Some(pcb.clone())
            } else {
                None
            }
        } else {
            self.select_group_signal_target(pcb.clone())
        };

        // Linux commits SIGNAL_GROUP_EXIT and its shared exit code under the
        // sighand lock before making a fatal signal observable to de_thread().
        // Otherwise the exec owner can see private SIGKILL pending, cancel
        // exec, and win the race to publish EAGAIN as the group exit status.
        if let Some(target_pcb) = target_pcb.as_ref() {
            if self.start_fatal_group_exit_if_needed(pcb.clone(), target_pcb.clone()) {
                return;
            }
        }

        if is_thread_target {
            // Thread-level signal: set the thread pending bitmap
            pcb.sig_info_mut()
                .sig_pending_mut()
                .signal_mut()
                .insert((*self).into());
        } else if !shared_bitmap_set {
            // Process-level signal: on the normal path the enqueue function has already set the
            // bitmap inside the sighand write critical section; only non-enqueueing fast paths such as SIGKILL / KTHREAD set it here.
            pcb.sighand().shared_pending_signal_insert(*self);
        }

        // 若目标进程存在 signalfd 监听该信号，需要唤醒其等待者/epoll。
        crate::ipc::signalfd::notify_signalfd_for_pcb(&pcb, *self);

        let Some(target_pcb) = target_pcb else {
            if is_thread_target {
                pcb.recalc_sigpending();
            }
            return;
        };

        target_pcb.recalc_sigpending();
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        signal_wake_up(target_pcb, *self == Signal::SIGKILL);
    }

    fn start_fatal_group_exit_if_needed(
        &self,
        suggested: Arc<ProcessControlBlock>,
        target: Arc<ProcessControlBlock>,
    ) -> bool {
        if !self.should_start_fatal_group_exit(&suggested, &target) {
            return false;
        }

        if !target
            .sighand()
            .start_group_exit_for_fatal_signal(*self as usize)
        {
            // Linux only performs the O(n) fatal broadcast for the first
            // transition to SIGNAL_GROUP_EXIT.  A later delivery still follows
            // complete_signal()'s ordinary pending/wakeup path; consuming it
            // here would make SIGKILL disappear without waking any task.
            return false;
        }

        // Preserve the old ordering: collect the group before the first wake
        // can let another CPU change membership.  The explicit target below
        // remains independent of this second group-leader lookup.
        let tasks = ProcessManager::thread_group_tasks_snapshot(target.clone());
        Self::queue_private_sigkill_to_thread(&target);
        for task in tasks {
            if Arc::ptr_eq(&task, &target) {
                continue;
            }
            Self::queue_private_sigkill_to_thread(&task);
        }

        true
    }

    fn should_start_fatal_group_exit(
        &self,
        suggested: &Arc<ProcessControlBlock>,
        target: &Arc<ProcessControlBlock>,
    ) -> bool {
        if SIG_KERNEL_COREDUMP_MASK.contains((*self).into()) {
            return false;
        }

        if !self.sig_fatal(suggested.clone()) {
            return false;
        }

        if *self != Signal::SIGKILL && suggested.is_ptraced() {
            return false;
        }

        if *self != Signal::SIGKILL && target.blocks_signal_for_fatal_group_exit(*self) {
            return false;
        }

        true
    }

    /// Queue the private SIGKILL used after the kernel has already committed
    /// to tearing down sibling threads for group-exit or de_thread().
    ///
    /// Unlike normal signal delivery, this must not run complete_signal()
    /// again: during de_thread() the exec owner is intentionally preserved,
    /// while during group-exit the shared exit state is already committed.
    pub(crate) fn queue_private_sigkill_to_thread(task: &Arc<ProcessControlBlock>) {
        if task.flags().contains(ProcessFlags::EXITING) || task.is_zombie() || task.is_dead() {
            return;
        }

        task.sig_info_mut()
            .sig_pending_mut()
            .signal_mut()
            .insert(Signal::SIGKILL.into());
        task.recalc_sigpending();
        signal_wake_up(task.clone(), true);
    }

    fn select_group_signal_target(
        &self,
        suggested: Arc<ProcessControlBlock>,
    ) -> Option<Arc<ProcessControlBlock>> {
        if self.wants_signal(suggested.clone()) {
            return Some(suggested);
        }

        let leader = ProcessManager::thread_group_leader_of(&suggested);
        // Zero-allocation probe: an empty group-task list means the leader is the only thread, so return without building a snapshot.
        if leader.threads_read_irqsave().group_tasks_is_empty() {
            return None;
        }
        let tasks = ProcessManager::thread_group_tasks_snapshot(leader.clone());

        let start = suggested
            .sighand()
            .curr_target()
            .unwrap_or_else(|| leader.clone());
        let start_index = tasks
            .iter()
            .position(|task| Arc::ptr_eq(task, &start))
            .unwrap_or(0);

        for offset in 0..tasks.len() {
            let idx = (start_index + offset) % tasks.len();
            let task = tasks[idx].clone();
            if self.wants_signal(task.clone()) {
                suggested.sighand().set_curr_target(&task);
                return Some(task);
            }
        }

        None
    }

    /// 本函数用于检测指定的进程是否想要接收SIG这个信号。
    ///
    /// 当我们对于进程组中的所有进程都运行了这个检查之后，我们将可以找到组内愿意接收信号的进程。
    /// 这么做是为了防止我们把信号发送给了一个正在或已经退出的进程，或者是不响应该信号的进程。
    #[inline]
    fn wants_signal(&self, pcb: Arc<ProcessControlBlock>) -> bool {
        let blocked = *pcb.sig_info_irqsave().sig_blocked();
        if blocked.contains((*self).into()) {
            return false;
        }

        // 若进程正在退出，则不能接收
        if pcb.flags().contains(ProcessFlags::EXITING) {
            return false;
        }
        if pcb.is_zombie() || pcb.is_dead() {
            return false;
        }

        // SIGKILL 总是唤醒
        if *self == Signal::SIGKILL {
            return true;
        }

        let state = pcb.sched_info().state();
        if state.is_stopped() {
            return false;
        }

        ProcessManager::is_current(&pcb) || !pcb.has_pending_signal_fast()
    }

    /// @brief 判断signal的处理是否可能使得整个进程组退出
    /// @return true 可能会导致退出（不一定）
    #[allow(dead_code)]
    #[inline]
    fn sig_fatal(&self, pcb: Arc<ProcessControlBlock>) -> bool {
        if !(self.into_sigset() & (SIG_KERNEL_IGNORE_MASK | SIG_KERNEL_STOP_MASK)).is_empty() {
            return false;
        }

        let sa = pcb.sighand().handler(*self).unwrap();
        let action = sa.action();
        // 如果handler是空，采用默认函数，signal处理可能会导致进程退出。
        match action {
            SigactionType::SaHandler(handler) => handler.is_sig_default(),
            SigactionType::SaSigaction(sigaction) => sigaction.is_none(),
        }
        // todo: 参照linux的sig_fatal实现完整功能
    }

    /// @brief 检查pcb状态、Init 属性、Handler 设置
    fn sig_task_ignored(&self, pcb: &Arc<ProcessControlBlock>, force: bool) -> bool {
        // init 进程忽略 SIGKILL 和 SIGSTOP，防止系统意外崩溃。
        if unlikely(pcb.raw_pid().data() == 1) && SIG_KERNEL_ONLY_MASK.contains(self.into_sigset())
        {
            return true;
        }
        let sighand = pcb.sighand();
        if let Some(sa) = sighand.handler(*self) {
            // 容器中的 init 进程 或者 被标记为 UNKILLABLE 的进程，如果Handler为默认且不是强制发送，永远不能忽略 SIGKILL 和 SIGSTOP
            let is_dfl = sa.is_default();
            if unlikely(sighand.flags_contains(SignalFlags::UNKILLABLE))
                && is_dfl
                && !(force && SIG_KERNEL_ONLY_MASK.contains(self.into_sigset()))
            {
                return true;
            }
            // sig_handler_ignored() 检查是否被设置为 IGNORE
            if sa.is_ignore() || (is_dfl && SIG_KERNEL_IGNORE_MASK.contains(self.into_sigset())) {
                return true;
            }
        }
        false
    }

    /// @brief 判断信号是否应该被忽略
    fn sig_ignored(&self, pcb: &Arc<ProcessControlBlock>, force: bool) -> bool {
        // 即使信号处理函数是 IGN，如果该信号被阻塞，它也必须留在队列中，直到解除了阻塞（此时 handler 可能已经变了）。
        let sig_info = pcb.sig_info_irqsave();
        if sig_info.sig_blocked().contains(self.into_sigset())
            || (pcb.flags().contains(ProcessFlags::RESTORE_SIG_MASK)
                && sig_info.saved_sigmask().contains(self.into_sigset()))
        {
            // log::debug!(
            //     "sig_ignored: signal {:?} is blocked, current sigblocked={:b}, saved_sigmask={:b}",
            //     self,
            //     sig_info.sig_blocked().bits(),
            //     sig_info.saved_sigmask().bits()
            // );
            return false;
        }
        drop(sig_info);

        // For a ptrace-attached task, no non-SIGKILL signal is treated as ignored -- the tracer must be able to observe every signal
        if pcb.flags().contains(ProcessFlags::PTRACED) && *self != Signal::SIGKILL {
            return false;
        }

        Self::sig_task_ignored(self, pcb, force)
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
        // 统一从线程组组长的 ThreadInfo 中获取完整线程列表。
        // 注意：当前 sighand 共享在 CLONE_THREAD 线程组内，因此标志位操作仍然只需要对共享 sighand 做一次。
        let thread_group_leader = ProcessManager::thread_group_leader_of(&pcb);

        // Linux prepare_signal() drops every signal except SIGKILL once group
        // exit has started. In particular, SIGCONT must not revive a group
        // whose job-control state was cleared by do_group_exit().
        if thread_group_leader
            .sighand()
            .flags_contains(SignalFlags::GROUP_EXIT)
        {
            return *self == Signal::SIGKILL;
        }

        let flush: SigSet;
        if !(self.into_sigset() & SIG_KERNEL_STOP_MASK).is_empty() {
            flush = Signal::SIGCONT.into_sigset();
            // Stop 类信号：清理 SIGCONT（共享 + 各线程私有 pending）
            thread_group_leader
                .sighand()
                .shared_pending_flush_by_mask(&flush);
            ProcessManager::for_each_thread_in_group(thread_group_leader.clone(), |t| {
                t.sig_info_mut().sig_pending_mut().flush_by_mask(&flush);
                true
            });
            // For a ptrace-attached tracee, stop signals are not eagerly group-stopped at the
            // sender; instead they are enqueued so the delivery-side ptrace_signal turns them
            // into a signal-delivery-stop; otherwise the tracee enters group-stop (not ptrace-stop)
            // and PTRACE_CONT fails with ESRCH. PTRACED is per-thread, so check the target pcb, not the leader.
            if pcb.flags().contains(ProcessFlags::PTRACED) {
                return !self.sig_ignored(&pcb, false);
            }
            // Not traced: transactional group-stop (atomically set STOP_STOPPED | CLD_STOPPED +
            // stop_signal while holding the sighand lock), matching Linux's sighand -> pi_lock
            // -> rq_lock ordering to prevent a concurrent SIGCONT from tearing the transition.
            let fresh_stop = thread_group_leader
                .sighand()
                .transition_group_stop(*self, || {
                    // stop_task only rejects an already Exited task. Publishing
                    // the group event therefore requires at least one live
                    // member; every live member in this stable snapshot is
                    // transitioned to Stopped before the callback returns.
                    let mut has_live_member = false;
                    ProcessManager::for_each_thread_in_group(thread_group_leader.clone(), |t| {
                        has_live_member |= ProcessManager::stop_task(&t).is_ok();
                        true
                    });
                    has_live_member
                });

            if fresh_stop {
                if let Some(parent) = pcb.parent_pcb() {
                    let _ = crate::ipc::kill::send_signal_to_pcb(parent.clone(), Signal::SIGCHLD);
                    parent.wake_all_waiters();
                }
                // 唤醒等待在该子进程/线程上的等待者
                thread_group_leader.wake_all_waiters();
                ProcessManager::for_each_thread_in_group(thread_group_leader.clone(), |t| {
                    t.wake_all_waiters();
                    true
                });
            }

            // SIGSTOP 是 kernel-only stop 信号：其效果是把线程组置为 stopped 并通知父进程，
            // 不应作为“可传递到用户态”的 pending 信号继续入队。
            // 否则在 SIGCONT 后可能错误地以 EINTR/ERESTART* 形式打断正在执行的系统调用（gVisor sigstop_test 即依赖这一点）。
            if *self == Signal::SIGSTOP {
                // DragonOS 采用异步 stop_task，因此需要在此处补上 schedule。
                let current = ProcessManager::current_pcb();
                let is_self_stop =
                    Arc::ptr_eq(&current, &pcb) || Arc::ptr_eq(&current, &thread_group_leader);
                if is_self_stop {
                    crate::sched::schedule(crate::sched::SchedMode::SM_NONE);
                }
                return false;
            }
        } else if *self == Signal::SIGCONT {
            flush = SIG_KERNEL_STOP_MASK;
            assert!(!flush.is_empty());
            // 清理 STOP 类挂起信号
            thread_group_leader
                .sighand()
                .shared_pending_flush_by_mask(&flush);
            ProcessManager::for_each_thread_in_group(thread_group_leader.clone(), |t| {
                t.sig_info_mut().sig_pending_mut().flush_by_mask(&flush);
                true
            });

            // SIGCONT always wakes stopped threads. Only a completed persistent
            // group stop produces a continued event.
            let was_stopped = thread_group_leader.sighand().transition_group_continue(|| {
                ProcessManager::for_each_thread_in_group(thread_group_leader.clone(), |t| {
                    // Non-seized threads are woken directly; waking seized threads is handled by ptrace_trap_notify.
                    if !t.flags().contains(ProcessFlags::PT_SEIZED)
                        && !t.ptrace_state.lock_irqsave().in_ptrace_stop
                    {
                        let _ = ProcessManager::wakeup_stop(&t);
                    }
                    true
                });
            });

            if was_stopped {
                if let Some(parent) = pcb.parent_pcb() {
                    let _ = crate::ipc::kill::send_signal_to_pcb(parent.clone(), Signal::SIGCHLD);
                    parent.wake_all_waiters();
                }
                // 唤醒等待在该子进程上的等待者
                thread_group_leader.wake_all_waiters();
                ProcessManager::for_each_thread_in_group(thread_group_leader.clone(), |t| {
                    t.wake_all_waiters();
                    true
                });
            }

            // A seized tracee receiving SIGCONT must leave group-stop/LISTEN and re-trap with PTRACE_EVENT_STOP
            ProcessManager::for_each_thread_in_group(thread_group_leader.clone(), |t| {
                if t.flags().contains(ProcessFlags::PT_SEIZED) {
                    t.ptrace_trap_notify();
                }
                true
            });
            // 如果未处于 stopped，则不生成 CLD_CONTINUED/不通知父进程。
            // SIGCONT 需要完成“继续运行”的语义，但若其在当前 handler 语义下会被忽略（默认忽略且未被阻塞），
            // 则不应继续入队为 pending，否则可能错误地打断可重启系统调用。
            return !self.sig_ignored(&pcb, _force);
        }

        //TODO 仿照 linux 中的prepare signal完善逻辑，linux 中还会根据例如当前进程状态(Existing)进行判断，现在的信号能否发出就只是根据 ignored 来判断
        return !self.sig_ignored(&pcb, _force);
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
fn signal_wake_up(pcb: Arc<ProcessControlBlock>, fatal: bool) {
    let state = pcb.sched_info().state();

    if fatal {
        // A fatal signal must interrupt both stops and interruptible sleeps. First leave
        // ptrace-stop: the ptrace_state lock serializes this with the tracer's request
        // validation / frame writes, so the tracer cannot write into a stale frame that has
        // already been woken. Note: clear the flags and release the lock before waking --
        // holding the lock across the wakeup reverse-nests with the scheduler lock inside it.
        {
            let mut ps = pcb.ptrace_state.lock_irqsave();
            if ps.frozen {
                ps.deferred_fatal_wake = true;
                return;
            }
            ps.in_ptrace_stop = false;
        }
        let r = if state.is_stopped() {
            ProcessManager::wakeup_stop(&pcb)
        } else if state.is_blocked_interruptable() {
            // Interruptible sleeps (including killable waits) are woken directly to take the exit path.
            ProcessManager::wakeup(&pcb)
        } else {
            // Running or in an uninterruptible sleep: waking does nothing useful, so only kick
            // to make it re-enter the kernel promptly to handle the fatal signal. Uninterruptible
            // sleeps are not force-woken, consistent with the "uninterruptible" semantics.
            ProcessManager::kick(&pcb);
            return;
        };
        // Kick only as a fallback when the wakeup fails; on success the wakeup path has already
        // rescheduled the target, so kicking again would be a redundant IPI.
        if r.is_err() {
            ProcessManager::kick(&pcb);
        }
        return;
    }

    // Non-fatal: only wake interruptible sleeps; resuming Stopped tasks is handled by the dedicated SIGCONT path, which is left untouched here.
    if state.is_blocked_interruptable() {
        if ProcessManager::wakeup(&pcb).is_err() {
            ProcessManager::kick(&pcb);
        }
        return;
    }
    // A running target needs a kick to process pending signals (including job-control stops) promptly.
    if !state.is_stopped() {
        ProcessManager::kick(&pcb);
    }
}

fn recalc_sigpending_and_wake(pcb: Arc<ProcessControlBlock>) {
    if pcb.recalc_sigpending_tsk() {
        signal_wake_up(pcb, false);
    }
}

fn has_pending_signals(sigset: &SigSet, blocked: &SigSet) -> bool {
    sigset.bits() & (!blocked.bits()) != 0
}

impl ProcessControlBlock {
    // Lock order rule: sighand -> sig_info. Never take sighand after sig_info.
    /// 按“线程 pending -> 进程 shared pending”的顺序取出一个可见信号。
    /// 该实现避免在持有 sig_info 锁时进入 sighand 锁，降低锁交叉风险。
    pub fn dequeue_pending_signal(&self, sig_mask: &SigSet) -> (Signal, Option<SigInfo>) {
        let res = {
            let mut siginfo = self.sig_info_mut();
            let res = siginfo.sig_pending_mut().dequeue_signal(sig_mask);
            if res.0 != Signal::INVALID {
                res
            } else {
                drop(siginfo);
                self.sighand().shared_pending_dequeue(sig_mask)
            }
        };
        self.recalc_sigpending();
        res
    }

    /// 重新计算线程的flag中的TIF_SIGPENDING位
    /// 参考: https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/signal.c?r=&mo=4806&fi=182#182
    pub fn recalc_sigpending(&self) {
        if !self.recalc_sigpending_tsk() {
            self.flags().remove(ProcessFlags::HAS_PENDING_SIGNAL);
        }
    }

    fn recalc_sigpending_tsk(&self) -> bool {
        let sighand = self.sighand();
        let sighand_guard = sighand.inner_read();
        let siginfo_guard = self.sig_info_irqsave();
        if has_pending_signals(
            &siginfo_guard.sig_pending().signal(),
            siginfo_guard.sig_blocked(),
        ) || has_pending_signals(
            &sighand_guard.shared_pending.signal(),
            siginfo_guard.sig_blocked(),
        ) {
            self.flags().insert(ProcessFlags::HAS_PENDING_SIGNAL);
            return true;
        }
        /*
         * We must never clear the flag in another thread, or in current
         * when it's possible the current syscall is returning -ERESTART*.
         * So we don't clear it here, and only callers who know they should do.
         */
        return false;
    }

    fn blocks_signal_for_fatal_group_exit(&self, sig: Signal) -> bool {
        let sigset = sig.into_sigset();
        let siginfo = self.sig_info_irqsave();
        siginfo.sig_blocked().contains(sigset) || siginfo.real_blocked().contains(sigset)
    }
}
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/sched/signal.h?fi=restore_saved_sigmask#547
pub fn restore_saved_sigmask() {
    if ProcessManager::current_pcb()
        .flags()
        .test_and_clear(ProcessFlags::RESTORE_SIG_MASK)
    {
        let saved = *ProcessManager::current_pcb()
            .sig_info_irqsave()
            .saved_sigmask();
        __set_current_blocked(&saved);
    }
    compiler_fence(Ordering::SeqCst);
}

pub fn restore_saved_sigmask_unless(interrupted: bool) {
    if interrupted {
        if !ProcessManager::current_pcb().has_pending_signal_fast() {
            log::warn!("restore_saved_sigmask_unless: interrupted, but has NO pending signal");
        }
    } else {
        restore_saved_sigmask();
    }
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/include/uapi/asm-generic/signal-defs.h#72
/// 对应SIG_BLOCK，SIG_UNBLOCK，SIG_SETMASK
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigHow {
    Block = 0,
    Unblock = 1,
    SetMask = 2,
}

impl TryFrom<i32> for SigHow {
    type Error = SystemError;
    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(SigHow::Block),
            1 => Ok(SigHow::Unblock),
            2 => Ok(SigHow::SetMask),
            _ => Err(SystemError::EINVAL),
        }
    }
}

fn __set_task_blocked(pcb: &Arc<ProcessControlBlock>, new_set: &SigSet) {
    //todo 还有一个对线程组是否为空的判断，进程组、线程组实现之后，需要更改这里。
    if pcb.has_pending_signal() {
        let mut newblocked = *new_set;
        let guard = pcb.sig_info_irqsave();
        newblocked.remove(*guard.sig_blocked());
        drop(guard);

        retarget_shared_pending(pcb.clone(), newblocked);
    }
    *pcb.sig_info_mut().sig_block_mut() = *new_set;
    pcb.recalc_sigpending();
}

fn __set_current_blocked(new_set: &SigSet) {
    let pcb = ProcessManager::current_pcb();
    /*
        如果当前pcb的sig_blocked和新的相等，那么就不用改变它。
        请注意，一个进程的sig_blocked字段不能被其他进程修改！
    */
    if pcb.sig_info_irqsave().sig_blocked().eq(new_set) {
        return;
    }
    __set_task_blocked(&pcb, new_set);
}

fn retarget_shared_pending(pcb: Arc<ProcessControlBlock>, which: SigSet) {
    // Linux 语义：当线程的 blocked 集发生变化（尤其是“新增屏蔽”）时，
    // 需要尝试把 shared_pending 中受影响的信号“重定向”给同一线程组内
    // 其他未屏蔽该信号的线程去处理。
    let mut retarget = pcb.sighand().shared_pending_signal().intersection(which);
    if retarget.is_empty() {
        return;
    }

    let tasks = ProcessManager::thread_group_tasks_snapshot(pcb.clone());
    if tasks.len() <= 1 {
        return;
    }

    let start_index = tasks
        .iter()
        .position(|task| Arc::ptr_eq(task, &pcb))
        .unwrap_or(0);

    for offset in 1..tasks.len() {
        let idx = (start_index + offset) % tasks.len();
        let task = tasks[idx].clone();
        if task.flags().contains(ProcessFlags::EXITING) {
            continue;
        }

        // 若该线程把 retarget 中的信号全部屏蔽，则它无法处理这些 shared_pending 信号
        let blocked = *task.sig_info_irqsave().sig_blocked();
        if retarget.difference(blocked).is_empty() {
            continue;
        }

        // 当前线程能处理的信号不需要再重定向给后续线程。
        retarget = retarget.intersection(blocked);

        if !task.has_pending_signal_fast() {
            recalc_sigpending_and_wake(task.clone());
        }

        if retarget.is_empty() {
            break;
        }
    }
    // debug!("retarget_shared_pending done!");
}

fn should_force_send(info: Option<&mut SigInfo>, target: &Arc<ProcessControlBlock>) -> bool {
    let Some(info) = info else {
        return false;
    };

    if matches!(info.sig_code(), SigCode::Kernel | SigCode::SysSeccomp) {
        return true;
    }

    if info.has_pid_and_uid() && signal_from_ancestor_pid_namespace(target) {
        info.clear_sender_pid();
        return true;
    }

    false
}

fn signal_from_ancestor_pid_namespace(target: &Arc<ProcessControlBlock>) -> bool {
    let current = ProcessManager::current_pcb();
    let Some(current_pid) = current.task_pid_ptr(PidType::PID) else {
        return false;
    };
    let Some(target_ns) = target.try_active_pid_ns() else {
        return true;
    };

    current_pid.pid_nr_ns(&target_ns).data() == 0
}

/// 设置当前进程的屏蔽信号 (sig_block)
///
/// ## 参数
///
/// - `new_set` 新的屏蔽信号bitmap的值
pub fn set_current_blocked(new_set: &mut SigSet) {
    let to_remove: SigSet =
        <Signal as Into<SigSet>>::into(Signal::SIGKILL) | Signal::SIGSTOP.into();
    new_set.remove(to_remove);
    __set_current_blocked(new_set);
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/signal.c?fi=set_user_sigmask#set_user_sigmask
/// 功能与set_current_blocked相同，多一步保存当前的sig_blocked到saved_sigmask
/// 由于这之中设置了saved_sigmask，因此从系统调用返回之前需要恢复saved_sigmask
pub fn set_user_sigmask(new_set: &mut SigSet) {
    let pcb = ProcessManager::current_pcb();
    let mut guard = pcb.sig_info_mut();
    let oset = *guard.sig_blocked();

    let flags = pcb.flags();
    flags.insert(ProcessFlags::RESTORE_SIG_MASK);

    let saved_sigmask = guard.saved_sigmask_mut();
    *saved_sigmask = oset;
    drop(guard);

    set_current_blocked(new_set);
}

/// 设置当前进程的屏蔽信号 (sig_block)
///
/// ## 参数
///
/// - `how` 设置方式
/// - `new_set` 新的屏蔽信号bitmap的值
pub fn set_sigprocmask(how: SigHow, set: SigSet) -> Result<SigSet, SystemError> {
    let pcb: Arc<ProcessControlBlock> = ProcessManager::current_pcb();
    let guard = pcb.sig_info_irqsave();
    let oset = *guard.sig_blocked();

    let mut res_set = oset;
    drop(guard);

    match how {
        SigHow::Block => {
            // log::debug!("SIG_BLOCK\tGoing to insert is: {:#x}", set.bits());
            res_set.insert(set);
        }
        SigHow::Unblock => {
            // log::debug!("SIG_UNBLOCK\tGoing to set is: {:#x}", set.bits());
            res_set.remove(set);
        }
        SigHow::SetMask => {
            // log::debug!("SIG_SETMASK\tGoing to set is: {:#x}", set.bits());
            res_set = set;
        }
    }

    res_set.remove(Signal::SIGKILL.into());
    res_set.remove(Signal::SIGSTOP.into());
    __set_current_blocked(&res_set);
    Ok(oset)
}

#[derive(Debug)]
pub struct RestartBlock {
    pub data: RestartBlockData,
    pub restart_fn: &'static dyn RestartFn,
}

impl RestartBlock {
    pub fn new(restart_fn: &'static dyn RestartFn, data: RestartBlockData) -> Self {
        Self { data, restart_fn }
    }
}

pub trait RestartFn: Debug + Sync + Send + 'static {
    fn call(&self, data: &mut RestartBlockData) -> Result<usize, SystemError>;
}

#[derive(Debug, Clone)]
pub enum RestartBlockData {
    Poll(PollRestartBlockData),
    Nanosleep {
        deadline: crate::time::PosixTimeSpec,
        clockid: crate::time::syscall::PosixClockID,
        rmtp: Option<VirtAddr>,
    },
    // todo: futex_wait
    FutexWait(),
}

impl RestartBlockData {
    pub fn new_poll(pollfd_ptr: VirtAddr, nfds: u32, timeout_instant: Option<Instant>) -> Self {
        Self::Poll(PollRestartBlockData {
            pollfd_ptr,
            nfds,
            timeout_instant,
        })
    }

    pub fn new_nanosleep(
        deadline: crate::time::PosixTimeSpec,
        clockid: crate::time::syscall::PosixClockID,
        rmtp: Option<VirtAddr>,
    ) -> Self {
        Self::Nanosleep {
            deadline,
            clockid,
            rmtp,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PollRestartBlockData {
    pub pollfd_ptr: VirtAddr,
    pub nfds: u32,
    pub timeout_instant: Option<Instant>,
}

fn ktime_now(clockid: PosixClockID) -> PosixTimeSpec {
    posix_clock_now(clockid)
}

fn calc_remaining(deadline: &PosixTimeSpec, now: &PosixTimeSpec) -> PosixTimeSpec {
    deadline.saturating_sub_timespec(now)
}

fn write_nanosleep_remaining(
    rmtp: Option<VirtAddr>,
    remaining: &PosixTimeSpec,
) -> Result<(), SystemError> {
    if let Some(rmtp) = rmtp {
        let mut writer = UserBufferWriter::new(
            rmtp.as_ptr::<PosixTimeSpec>(),
            core::mem::size_of::<PosixTimeSpec>(),
            true,
        )?;
        writer.copy_one_to_user(remaining, 0)?;
    }
    Ok(())
}

/// Nanosleep 的重启函数：根据保存的 deadline/clockid 继续等待或重启
#[derive(Debug)]
pub struct RestartFnNanosleep;

impl RestartFn for RestartFnNanosleep {
    fn call(&self, data: &mut RestartBlockData) -> Result<usize, SystemError> {
        if let RestartBlockData::Nanosleep {
            deadline,
            clockid,
            rmtp,
        } = data
        {
            if !deadline.is_valid_timeout() || *clockid == PosixClockID::ThreadCPUTimeID {
                return Err(SystemError::EINVAL);
            }
            let deadline_ns = (deadline.tv_sec as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(deadline.tv_nsec as u64);
            let wait_res = match *clockid {
                PosixClockID::ProcessCPUTimeID => {
                    let current = ProcessManager::current_pcb();
                    let leader = if current.is_thread_group_leader() {
                        current
                    } else {
                        current
                            .threads_read_irqsave()
                            .group_leader()
                            .unwrap_or_else(ProcessManager::current_pcb)
                    };
                    if leader.process_cputime_ns() >= deadline_ns {
                        Ok(())
                    } else {
                        leader.cputime_wait_queue().wait_event_interruptible(
                            || leader.process_cputime_ns() >= deadline_ns,
                            None::<fn()>,
                        )
                    }
                }
                PosixClockID::ThreadCPUTimeID => return Err(SystemError::EINVAL),
                _ => {
                    let now = ktime_now(*clockid);
                    let remain = calc_remaining(deadline, &now);
                    if remain.tv_sec == 0 && remain.tv_nsec == 0 {
                        Ok(())
                    } else {
                        nanosleep(remain)
                    }
                }
            };

            match wait_res {
                Ok(()) => return Ok(0),
                Err(SystemError::ERESTARTSYS) => {
                    let remaining = calc_remaining(deadline, &ktime_now(*clockid));
                    if remaining.is_empty() {
                        return Ok(0);
                    }
                    write_nanosleep_remaining(*rmtp, &remaining)?;
                }
                Err(e) => return Err(e),
            }
            let rb = RestartBlock::new(&RestartFnNanosleep, data.clone());
            return crate::process::ProcessManager::current_pcb().set_restart_fn(Some(rb));
        }
        panic!("RestartFnNanosleep called with wrong data type: {:?}", data);
    }
}
