use crate::arch::interrupt::TrapFrame;
use crate::arch::ipc::signal::{SigFlags, Signal};
use crate::arch::kprobe;
use crate::ipc::signal_types::{
    ChldCode, OriginCode, SigChldInfo, SigCode, SigFaultInfo, SigInfo, SigType, Sigaction,
    SigactionType, SignalFlags,
};
use crate::process::{
    pid::PidType, ProcessControlBlock, ProcessFlags, ProcessManager, ProcessState, PtraceEvent,
    PtraceOptions, PtraceRequest, PtraceStopReason, PtraceSyscallInfo, PtraceSyscallInfoData,
    PtraceSyscallInfoEntry, PtraceSyscallInfoExit, PtraceSyscallInfoOp, RawPid,
};
use crate::sched::{schedule, DequeueFlag, EnqueueFlag, SchedMode, WakeupFlags};
use alloc::{sync::Arc, vec::Vec};
use core::{intrinsics::unlikely, mem::MaybeUninit};
use system_error::SystemError;

/// 在 get_signal 中调用的 ptrace 信号拦截器。
/// 它会使进程停止，并根据追踪者的指令决定如何处理信号。
/// 返回值:
/// - Some(Signal): 一个需要立即处理的信号。
/// - None: 信号被 ptrace 取消或重新排队了，当前无需处理。
pub fn ptrace_signal(
    pcb: &Arc<ProcessControlBlock>,
    original_signal: Signal,
    info: &mut Option<SigInfo>,
) -> Option<Signal> {
    log::debug!(
        "[PTRACE_SIGNAL] PID {} processing signal {:?}",
        pcb.raw_pid(),
        original_signal
    );

    // todo pcb.jobctl_set(JobControlFlags::STOP_DEQUEUED);
    // 核心：调用 ptrace_stop 使进程停止并等待追踪者。
    // ptrace_stop 会返回追踪者注入的信号。
    // 注意：ptrace_stop 内部会处理锁的释放和重新获取。
    let mut signr = pcb.ptrace_stop(original_signal as usize, ChldCode::Trapped, info.as_mut());

    log::debug!(
        "[PTRACE_SIGNAL] PID {} tracer returned signal {}",
        pcb.raw_pid(),
        signr
    );

    // 按照 Linux 6.6.21 的 get_signal 语义：
    // 如果 signr == 0，表示没有注入信号，应该丢弃原始信号，继续执行
    // 如果 signr != 0，表示注入了新信号，应该处理该信号
    if signr == 0 {
        log::debug!(
            "[PTRACE_SIGNAL] PID {} no injected signal, discarding original signal {:?}",
            pcb.raw_pid(),
            original_signal
        );
        return None; // 丢弃原始信号，继续处理下一个信号（如果没有，则继续执行）
    }
    // 将注入的信号转换为 Signal 类型
    let mut injected_signal = Signal::from(signr);
    if injected_signal == Signal::INVALID {
        // 这不应该发生，因为 signr != 0
        log::error!(
            "[PTRACE_SIGNAL] PID {} invalid signal {}",
            pcb.raw_pid(),
            signr
        );
        return None;
    }

    // 如果追踪者注入了不同于原始信号的新信号，更新 siginfo。
    // 注意：按照 Linux 6.6.21 的 ptrace 语义，注入的信号应该使用 SI_USER 代码
    // 而不是 SigChld 类型，因为这不是真正的子进程状态变化
    if injected_signal != original_signal {
        log::debug!(
            "[PTRACE_SIGNAL] PID {} tracer injected new signal {:?} (was {:?})",
            pcb.raw_pid(),
            injected_signal,
            original_signal
        );
        if let Some(info_ref) = info {
            let tracer = pcb.parent_pcb().unwrap();
            // 使用 Kill 类型（SI_USER）而不是 SigChld
            // 这样更符合 ptrace 注入信号的语义
            *info_ref = SigInfo::new(
                injected_signal,
                0,
                SigCode::Origin(OriginCode::User),
                SigType::Kill {
                    pid: tracer.raw_pid(),
                    uid: tracer.cred().uid.data() as u32,
                },
            );
        }
    }

    // 特殊处理 SIGCONT：需要清除挂起的停止信号，但仍然要传递给用户
    // 按照 Linux 6.6.21 的语义，SIGCONT 应该唤醒进程并传递给用户空间处理
    if injected_signal == Signal::SIGCONT {
        log::debug!(
            "[PTRACE_SIGNAL] PID {} injected SIGCONT, clearing any pending stop signals",
            pcb.raw_pid()
        );
        // 清除任何挂起的停止信号（如 SIGSTOP, SIGTSTP 等）
        let mut sig_info = pcb.sig_info.write();
        let pending = sig_info.sig_pending_mut().signal_mut();
        // 清除停止信号
        for stop_sig in [
            Signal::SIGSTOP,
            Signal::SIGTSTP,
            Signal::SIGTTIN,
            Signal::SIGTTOU,
        ] {
            pending.remove(stop_sig.into());
        }
        drop(sig_info);
        // 返回 Some(injected_signal) 让 SIGCONT 被正常处理
        // 这样用户的信号处理函数（如 sigcont_handler）会被调用
        return Some(injected_signal);
    }

    // 检查新信号是否被当前进程的信号掩码阻塞
    let sig_info_guard = pcb.sig_info_irqsave();
    if sig_info_guard
        .sig_blocked()
        .contains(injected_signal.into())
    {
        log::debug!(
            "[PTRACE_SIGNAL] PID {} signal {:?} is blocked, requeuing",
            pcb.raw_pid(),
            injected_signal
        );
        // 如果被阻塞了，则将信号重新排队，让它在未来被处理。
        let _ =
            injected_signal.send_signal_info_to_pcb(info.as_mut(), Arc::clone(pcb), PidType::PID);
        // 告诉 get_signal，当前没有需要立即处理的信号。
        return None;
    }
    // 如果没有被阻塞，则返回这个新信号，让 get_signal 继续分发和处理它。
    log::debug!(
        "[PTRACE_SIGNAL] PID {} returning signal {:?} to handle",
        pcb.raw_pid(),
        injected_signal
    );
    Some(injected_signal)
}

pub fn do_notify_parent(child: &ProcessControlBlock, signal: Signal) -> Result<bool, SystemError> {
    let parent = match child.parent_pcb() {
        Some(p) => p,
        None => {
            // 父进程已经退出，子进程已被 `init` 收养
            return Err(SystemError::ESRCH);
        }
    };
    // debug_assert!(!child.is_stopped_or_traced());
    // todo WARN_ON_ONCE(!tsk->ptrace && (tsk->group_leader != tsk || !thread_group_empty(tsk)));
    let mut autoreap = false;
    let mut effective_signal = Some(signal);
    // 检查父进程的信号处理方式以确定是否自动回收
    {
        let sighand_lock = parent.sighand();
        let sa = sighand_lock.handler(Signal::SIGCHLD);
        // 这里简化了 !ptrace 的检查
        if signal == Signal::SIGCHLD {
            if sa.map(|s| s.action().is_ignore()).unwrap_or(true) {
                // 父进程忽略 SIGCHLD，子进程应被自动回收
                autoreap = true;
                // 并且不发送信号
                effective_signal = None;
            } else if sa
                .map(|s| s.flags().contains(SigFlags::SA_NOCLDWAIT))
                .unwrap_or(true)
            {
                // 父进程不等待子进程，子进程应被自动回收
                autoreap = true;
                // 但根据POSIX，信号仍然可以发送
            }
        }
    }
    if let Some(sig) = effective_signal {
        let mut info = SigInfo::new(
            sig,
            0,
            SigCode::Origin(OriginCode::Kernel),
            SigType::SigChld(SigChldInfo {
                pid: child.task_pid_vnr(),
                uid: child.cred().uid.data(),
                status: 0, // todo
                utime: 0,  // 可以根据需要填充实际值
                stime: 0,  // 可以根据需要填充实际值
            }),
        );
        sig.send_signal_info_to_pcb(Some(&mut info), parent, PidType::TGID)?;
    }
    // 因为即使父进程忽略信号，也可能在 wait() 中阻塞，需要被唤醒以返回 -ECHILD
    child.wake_up_parent(None);
    Ok(autoreap)
}

pub fn handle_ptrace_signal_stop(current_pcb: &Arc<ProcessControlBlock>, sig: Signal) {
    let mut ptrace_state = current_pcb.ptrace_state.lock();
    ptrace_state.stop_reason = PtraceStopReason::Signal(sig);
    ptrace_state.exit_code = sig as usize;
    log::debug!(
        "PID {} stopping due to ptrace on signal {:?}",
        current_pcb.raw_pid(),
        sig
    );
    drop(ptrace_state);

    // 调用 ptrace_stop 让进程真正停止并等待跟踪器
    // 这会设置状态、通知跟踪器、并调用 schedule() 让出 CPU
    let mut info = SigInfo::new(
        sig,
        0,
        SigCode::Origin(OriginCode::Kernel),
        SigType::SigFault(SigFaultInfo { addr: 0, trapno: 0 }),
    );
    current_pcb.ptrace_stop(sig as usize, ChldCode::Stopped, Some(&mut info));
}

impl ProcessControlBlock {
    /// 设置ptrace跟踪器
    pub fn set_tracer(&self, tracer: RawPid) -> Result<(), SystemError> {
        // 确保当前没有被追踪
        if self.ptrace_state.lock().tracer.is_some() {
            return Err(SystemError::EPERM);
        }
        // 设置跟踪关系
        let mut state = self.ptrace_state.lock();
        state.tracer = Some(tracer);
        // 设置 PTRACED 标志
        self.flags().insert(ProcessFlags::PTRACED);
        Ok(())
    }

    /// 移除ptrace跟踪器
    pub fn clear_tracer(&self) {
        self.ptrace_state.lock().tracer = None;
        self.flags()
            .remove(ProcessFlags::PTRACED | ProcessFlags::TRACE_SYSCALL);
    }

    /// 获取ptrace跟踪器
    pub fn tracer(&self) -> Option<RawPid> {
        self.ptrace_state.lock().tracer.clone()
    }

    pub fn is_traced(&self) -> bool {
        self.ptrace_state.lock().tracer.is_some()
    }

    pub fn is_traced_by(&self, tracer: &Arc<ProcessControlBlock>) -> bool {
        match self.tracer() {
            Some(tracer_pid) => tracer_pid == tracer.raw_pid(),
            None => false,
        }
    }

    pub fn set_state(&self, state: ProcessState) {
        let mut sched_info = self.sched_info.inner_lock_write_irqsave();
        sched_info.set_state(state);
    }

    /// 获取原始父进程 PID（非跟踪器）
    pub fn real_parent_pid(&self) -> Option<RawPid> {
        // 这里需要根据您的实际实现返回原始父进程 PID
        // 假设有一个字段存储原始父进程
        self.parent_pcb().map(|p| p.raw_pid())
    }

    /// 获取父进程 PID（确保总是返回有效值）
    pub fn parent_pid(&self) -> RawPid {
        // 1. 尝试从直接父进程引用获取
        if let Some(tracer) = self.tracer() {
            return tracer;
        }
        if let Some(parent) = self.parent_pcb() {
            return parent.raw_pid();
        }
        // // 2. 尝试从进程基本信息中的 ppid 字段获取
        // if self.basic().ppid != Pid(0) {
        //     return Pid::new(self.basic().ppid.data() as u32);
        // }
        // // 3. 如果都没有，则返回 init 进程的 PID (1)
        self.raw_pid()
    }

    pub fn set_parent(&self, new_parent: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        if new_parent.raw_pid() == self.raw_pid() {
            return Err(SystemError::EINVAL); // 不能将自己设为父进程
        }
        if new_parent.is_exited() {
            return Err(SystemError::ESRCH); // 父进程不能是退出状态或僵尸状态
        }

        let old_parent = self.parent_pcb().map(|p| p.raw_pid());
        log::debug!(
            "[SET_PARENT] PID {} changing parent from {:?} to {}",
            self.raw_pid(),
            old_parent,
            new_parent.raw_pid()
        );

        *(self.parent_pcb.write()) = Arc::downgrade(new_parent);

        // 验证设置是否成功
        let verify_parent = self.parent_pcb().map(|p| p.raw_pid());
        log::debug!(
            "[SET_PARENT] PID {} verified parent_pcb={:?}",
            self.raw_pid(),
            verify_parent
        );

        Ok(())
    }

    /// 获取停止状态的状态字
    pub fn ptrace_status_code(&self) -> usize {
        self.ptrace_state.lock().status_code()
    }

    /// 添加信号到队列
    pub fn enqueue_signal(&self, signal: Signal) {
        let mut info = self.sig_info.write();
        info.sig_pending.signal_mut().insert(signal.into());
    }
    /// 从队列获取信号
    // pub fn dequeue_signal(&self) -> Option<Signal> {
    //     let mut info = self.sig_info.write();
    //     info.dequeue_signal(signal, self)
    // }

    /// 唤醒父进程的等待队列
    fn wake_up_parent(&self, state: Option<ProcessState>) {
        if let Some(parent) = self.parent_pcb() {
            parent.wait_queue.wakeup(state);
        }
    }

    /// 通知父进程（调试器）发送 SIGTRAP 信号并设置适当的退出代码。
    pub fn ptrace_notify(exit_code: usize) -> Result<(), SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        if (exit_code & (0x7f | !0xffff)) != Signal::SIGTRAP as usize {
            return Err(SystemError::EINVAL);
        }
        // 获取信号处理锁
        let sighand_lock = current_pcb.sighand();
        let result = Self::ptrace_do_notify(Signal::SIGTRAP, exit_code, None);
        drop(sighand_lock);
        result
    }

    /// 发送信号并通知父进程
    fn ptrace_do_notify(
        signal: Signal,
        exit_code: usize,
        _reason: Option<i32>, // todo
    ) -> Result<(), SystemError> {
        let current_pcb: Arc<ProcessControlBlock> = ProcessManager::current_pcb();
        // current_pcb.set_exit_code(exit_code);
        let mut info = SigInfo::new(
            signal,
            0,
            SigCode::Origin(OriginCode::Kernel),
            SigType::SigChld(SigChldInfo {
                pid: current_pcb.raw_pid(),
                uid: current_pcb.cred().uid.data(),
                status: exit_code as i32,
                utime: 0, // 可以根据需要填充实际值
                stime: 0, // 可以根据需要填充实际值
            }),
        );
        current_pcb.ptrace_stop(exit_code, ChldCode::Trapped, Some(&mut info));
        Ok(())
    }

    pub fn ptrace_event_enabled(&self, event: PtraceEvent) -> bool {
        let event_flag = 1 << (event as u32 + 3);
        self.ptrace_state.lock().event_message == event_flag && event_flag != 0;
        true
    }

    pub fn ptrace_event(&self, event: PtraceEvent, message: usize) {
        if unlikely(self.ptrace_event_enabled(event)) {
            self.ptrace_state.lock().event_message = message;
            let _ = Self::ptrace_notify((event as usize) << 8 | Signal::SIGTRAP as usize);
        } else if event == PtraceEvent::Exec {
            // if (ptrace_flags & (PT_PTRACED | PT_SEIZED)) == PT_PTRACED {
            log::debug!("ProcessFlags::PTRACED");
            let sig = Signal::SIGTRAP;
            let mut info = SigInfo::new(
                sig,
                0,
                SigCode::Origin(OriginCode::Kernel),
                SigType::SigFault(SigFaultInfo { addr: 0, trapno: 0 }),
            );
            let _ = sig.send_signal_info_to_pcb(
                Some(&mut info),
                self.self_ref.upgrade().unwrap(),
                PidType::PID,
            );
            // }
        }
        let wait_status = ((event as usize) << 8) | (Signal::SIGTRAP as usize);
        self.set_state(ProcessState::Runnable);
    }
    /// 设置进程为停止状态
    pub fn ptrace_stop(
        &self,
        exit_code: usize,
        why: ChldCode,
        info: Option<&mut SigInfo>,
    ) -> usize {
        log::debug!(
            "[PTRACE_STOP] PID {} stopping: exit_code={}, why={:?}",
            self.raw_pid(),
            exit_code,
            why
        );

        // 按照 Linux 6.6.21 的 ptrace_stop 语义：
        // 1. 设置 TRAPPING 标志，表示 tracee 正在停止
        // 2. 设置进程状态为 Stopped
        // 3. 唤醒 tracer 的等待队列
        // 4. 进入 schedule() 让出 CPU
        // 5. 当 tracer 唤醒 tracee 后，schedule() 返回
        // 6. 清除 TRAPPING 标志

        // 设置 TRAPPING 标志，表示正在停止
        self.flags().insert(ProcessFlags::TRAPPING);

        // self.last_siginfo = info.cloned();
        // 按照 Linux 6.6.21 的 ptrace_stop 语义：
        // 使用 TracedStopped 状态（类似于 Linux 的 TASK_TRACED）
        // 而不是 Stopped 状态（类似于 Linux 的 TASK_STOPPED）
        // 这确保 tracee 只能被 tracer 通过 ptrace_resume 唤醒

        // 获取 sched_info 锁并设置状态
        let mut sched_info = self.sched_info.inner_lock_write_irqsave();
        sched_info.set_state(ProcessState::TracedStopped(exit_code));
        // 关键：必须设置 sleep 标志，否则调度器不会把进程从运行队列移除
        sched_info.set_sleep();
        drop(sched_info);

        self.flags().insert(ProcessFlags::PTRACED);

        log::debug!(
            "[PTRACE_STOP] PID {} state set to TracedStopped({}), will call schedule",
            self.raw_pid(),
            exit_code
        );

        // 为下次stop恢复
        self.ptrace_state.lock().event_message = 0;

        // 按照 Linux 6.6.21 的 ptrace_stop 语义：
        // 1. 先通知 tracer (do_notify_parent_cldstop) - 在 schedule() 之前
        // 2. 清除 TRAPPING 标志
        // 3. 确保任务从运行队列移除
        // 4. 调用 schedule() 让出 CPU
        //
        // 这样 tracer 会被唤醒并可以 wait()，tracee 会进入睡眠状态

        // 通知跟踪器 - 必须在 schedule() 之前调用！
        if let Some(tracer) = self.parent_pcb() {
            log::debug!(
                "[PTRACE_STOP] PID {} notifying tracer={}",
                self.raw_pid(),
                tracer.raw_pid()
            );
            self.notify_tracer(&tracer, why);
        } else {
            log::error!("PID {} is traced but has no parent tracer!", self.raw_pid());
        }

        // 清除 TRAPPING 标志，表示已经完成停止准备工作
        self.flags().remove(ProcessFlags::TRAPPING);

        // 关键修复：确保任务从运行队列中移除
        // 由于任务可能之前被 signal_wake_up 唤醒并加入运行队列
        // 我们需要确保它现在被移除，否则 schedule() 会立即返回
        {
            let rq = crate::sched::cpu_rq(crate::smp::core::smp_get_processor_id().data() as usize);
            let (rq, _guard) = rq.self_lock();
            rq.deactivate_task(
                self.self_ref.upgrade().unwrap(),
                DequeueFlag::DEQUEUE_SLEEP | DequeueFlag::DEQUEUE_NOCLOCK,
            );
            log::debug!(
                "[PTRACE_STOP] PID {} manually deactivated from run queue",
                self.raw_pid()
            );
        }

        let preempt_count_before = ProcessManager::current_pcb().preempt_count();
        log::debug!(
            "[PTRACE_STOP] PID {} before schedule: preempt_count={}",
            self.raw_pid(),
            preempt_count_before
        );

        log::debug!(
            "[PTRACE_STOP] PID {} scheduling out, waiting for tracer",
            self.raw_pid()
        );

        // 调度出去，让出CPU等待跟踪器恢复
        // set_state 已经释放了 sched_info 锁，现在可以安全调用 schedule
        let state_before = self.sched_info().inner_lock_read_irqsave().state();
        log::debug!(
            "[PTRACE_STOP] PID {} before schedule: state={:?}",
            self.raw_pid(),
            state_before
        );
        drop(state_before);

        schedule(SchedMode::SM_NONE);

        // 从 schedule() 返回后，tracer 已经通过 ptrace_resume 唤醒了我们
        // 状态已经被 ptrace_resume 设置为 Runnable
        // 不要在这里手动修改状态！否则会导致竞态条件
        let state_after = self.sched_info().inner_lock_read_irqsave().state();
        log::debug!(
            "[PTRACE_STOP] PID {} after schedule: state={:?}",
            self.raw_pid(),
            state_after
        );
        drop(state_after);

        log::debug!(
            "[PTRACE_STOP] PID {} resumed by tracer, checking ptrace flag",
            self.raw_pid()
        );

        // 按照 Linux 6.6.21 的 ptrace_stop 语义：
        // 进程恢复后，应该返回 tracer 注入的信号（data 参数）
        // 而不是原始的 exit_code
        // 从 ptrace_state.injected_signal 读取 tracer 注入的信号
        let mut ptrace_state = self.ptrace_state.lock();
        let injected_signal = ptrace_state.injected_signal;

        log::debug!(
            "[PTRACE_STOP] PID {} injected_signal={:?} (original exit_code={})",
            self.raw_pid(),
            injected_signal,
            exit_code
        );

        // 按照 Linux 6.6.21 的 get_signal/ptrace_stop 语义：
        // 1. 如果注入的信号是 INVALID，返回 0，表示没有注入信号
        // 2. get_signal 会检查该信号，如果是 0，会丢弃原始信号并继续执行
        //    如果是非 0 信号，会处理该信号
        let result = if injected_signal == Signal::INVALID {
            log::debug!(
                "[PTRACE_STOP] PID {} no injected signal, continuing execution",
                self.raw_pid()
            );
            // 返回 0 表示没有注入信号，get_signal 会继续处理
            0
        } else {
            log::debug!(
                "[PTRACE_STOP] PID {} returning injected signal {:?}",
                self.raw_pid(),
                injected_signal
            );
            // 清除注入的信号，因为已经被处理了
            ptrace_state.injected_signal = Signal::INVALID;
            injected_signal as usize
        };
        drop(ptrace_state);

        result
    }

    fn notify_tracer(&self, tracer: &Arc<ProcessControlBlock>, why: ChldCode) {
        log::debug!(
            "[NOTIFY_TRACER] Notifying tracer={} about tracee={}, why={:?}",
            tracer.raw_pid(),
            self.raw_pid(),
            why
        );
        let status = match why {
            ChldCode::Stopped => self.exit_code().unwrap_or(0) as i32 & 0x7f,
            ChldCode::Trapped => self.exit_code().unwrap_or(0) as i32 & 0x7f,
            _ => Signal::SIGCONT as i32,
        };
        let mut info = SigInfo::new(
            Signal::SIGCHLD,
            0,
            SigCode::SigChld(why),
            SigType::SigChld(SigChldInfo {
                pid: self.raw_pid(),
                uid: self.cred().uid.data(),
                status,
                utime: 0, // todo: 填充时间
                stime: 0,
            }),
        );
        let should_send = {
            let tracer_sighand = tracer.sighand();
            let sa = tracer_sighand.handler(Signal::SIGCHLD);
            if let Some(sa) = sa {
                !sa.action().is_ignore() && !sa.flags().contains(SigFlags::SA_NOCLDSTOP)
            } else {
                // 当 sa 为 None 时，使用默认行为忽略
                false
            }
        };
        if should_send {
            Signal::SIGCHLD.send_signal_info_to_pcb(
                Some(&mut info),
                Arc::clone(tracer),
                PidType::TGID,
            );
        }
        // 按照 Linux 6.6.21 的 do_notify_parent_cldstop 语义：
        // __wake_up_parent 唤醒的是 parent 的 wait_chldexit 队列
        // 而不是 child 的队列
        // 这样 do_wait 中的 wait_event_interruptible 才能被唤醒
        log::debug!("[NOTIFY_TRACER] Waking up tracer's wait_queue");
        tracer
            .wait_queue
            .wakeup(Some(ProcessState::TracedStopped(status as usize)));
    }

    /// 检查进程是否可以被指定进程跟踪
    pub fn has_permission_to_trace(&self, _tracee: &Self) -> bool {
        // // 1. 超级用户可以跟踪任何进程
        // if self.is_superuser() {
        //     return true;
        // }
        // // 2. 检查是否拥有CAP_SYS_PTRACE权限
        // if self.cred().has_cap(Capability::CAP_SYS_PTRACE) {
        //     return true;
        // }
        // // 3. 检查用户ID是否相同
        // if self.basic().uid() == tracee.basic().uid() {
        //     return true;
        // }
        // false
        true
    }

    pub fn ptrace_link(&self, tracer: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        log::debug!(
            "[PTRACE_LINK] Linking tracer={} to tracee={}",
            tracer.raw_pid(),
            self.raw_pid()
        );

        if !tracer.has_permission_to_trace(self) {
            log::debug!(
                "[PTRACE_LINK] Permission denied: tracer={} cannot trace tracee={}",
                tracer.raw_pid(),
                self.raw_pid()
            );
            return Err(SystemError::EPERM);
        }

        log::debug!("[PTRACE_LINK] Setting tracer for tracee={}", self.raw_pid());
        // 将子进程添加到父进程的跟踪列表
        // let mut ptrace_list = tracer.ptraced_list.write();
        // let child_pid = self.raw_pid();
        // if ptrace_list.iter().any(|&pid| pid == child_pid) {
        //     return Err(SystemError::EALREADY);
        // }
        // ptrace_list.push(child_pid);
        self.set_tracer(tracer.raw_pid())?;

        log::debug!(
            "[PTRACE_LINK] Reparenting tracee={} to tracer={}",
            self.raw_pid(),
            tracer.raw_pid()
        );
        self.set_parent(tracer)?;

        log::debug!(
            "[PTRACE_LINK] Updating credentials for tracee={}",
            self.raw_pid()
        );
        *self.cred.lock() = tracer.cred().clone();

        log::debug!(
            "[PTRACE_LINK] Link completed: tracer={} -> tracee={}",
            tracer.raw_pid(),
            self.raw_pid()
        );

        Ok(())
    }

    pub fn ptrace_unlink(&self) -> Result<(), SystemError> {
        // 确保当前进程确实被跟踪
        if !self.is_traced() {
            return Err(SystemError::EINVAL);
        }
        // 清除系统调用跟踪相关工作
        // self.clear_syscall_trace_work();
        // 恢复父进程为真实父进程
        let real_parent = self.real_parent_pcb().ok_or(SystemError::ESRCH)?;
        self.set_parent(&real_parent)?;
        // 从跟踪器的跟踪列表中移除当前进程
        // let mut ptrace_list = tracer.ptraced_list.write();
        // if let Some(pos) = ptrace_list.iter().position(|&pid| pid == self.raw_pid()) {
        //     ptrace_list.remove(pos);
        // }
        // 清理凭证信息
        {
            let mut cred = self.cred.lock();
            // todo *cred = self.original_cred().clone();
        }
        // 获取信号锁保护信号相关操作
        let sighand_lock = self.sighand();
        self.clear_tracer();
        self.flags()
            .remove(ProcessFlags::PTRACED | ProcessFlags::TRACE_SYSCALL);
        // 清除所有挂起的陷阱和TRAPPING状态
        // self.clear_jobctl_pending(JobCtl::TRAP_MASK); // 假设有JobCtl枚举和clear_jobctl_pending方法
        // self.clear_jobctl_trapping(); // 假设有clear_jobctl_trapping方法
        // 如果进程没有退出且有停止信号或组停止计数，重新设置停止挂起标志
        // if !self.is_exiting()
        //     && (self.signal_flags().contains(SignalFlags::STOP_STOPPED)
        //         || self.group_stop_count() > 0)
        // {
        //     self.set_jobctl_pending(JobCtl::STOP_PENDING);
        //     // 如果没有设置停止信号掩码，默认使用SIGSTOP
        //     if !self.jobctl().contains(JobCtl::STOP_SIGMASK) {
        //         self.set_jobctl_pending(JobCtl::from_signal(Signal::SIGSTOP)); // 假设有from_signal方法
        //     }
        // }
        // 如果有停止挂起或任务处于被跟踪状态，唤醒进程
        // if self.jobctl().contains(JobCtl::STOP_PENDING) || self.is_traced() {
        //     self.ptrace_signal_wake_up(true); // 假设有ptrace_signal_wake_up方法
        // }
        drop(sighand_lock);
        Ok(())
    }

    /// 处理PTRACE_TRACEME请求
    pub fn traceme(&self) -> Result<isize, SystemError> {
        if self.is_traced() {
            return Err(SystemError::EPERM);
        }
        let parent = self.real_parent_pcb().ok_or(SystemError::ESRCH)?;
        self.flags().insert(ProcessFlags::PTRACED);
        self.ptrace_link(&parent)?;

        // 注意：不要修改 exit_signal！
        // exit_signal 是用来表示进程退出时发送给父进程的信号（通常是 SIGCHLD）
        // ptrace 注入的信号应该存储在 ptrace_state.injected_signal 中

        Ok(0)
    }

    /// 处理PTRACE_ATTACH请求
    pub fn attach(&self, tracer: &Arc<ProcessControlBlock>) -> Result<isize, SystemError> {
        log::debug!(
            "[PTRACE_ATTACH] Starting attach: tracer={}, target={}",
            tracer.raw_pid(),
            self.raw_pid()
        );

        // 验证权限（简化版）
        // 按照 Linux 6.6.21 的 ptrace_attach 语义：
        // 1. 不能 attach 自己
        // 2. 不能 attach 同一个线程组的其他线程
        let is_same_process = tracer.raw_pid() == self.raw_pid();
        let is_same_thread_group = tracer.raw_tgid() == self.raw_tgid();

        log::debug!(
            "[PTRACE_ATTACH] Permission check: tracer_pid={}, tracer_tgid={}, target_pid={}, target_tgid={}, same_process={}, same_thread_group={}",
            tracer.raw_pid(),
            tracer.raw_tgid(),
            self.raw_pid(),
            self.raw_tgid(),
            is_same_process,
            is_same_thread_group
        );

        if !tracer.has_permission_to_trace(self)
            || self.flags().contains(ProcessFlags::KTHREAD)
            || is_same_thread_group
        {
            log::debug!(
                "[PTRACE_ATTACH] Permission check failed for target={}",
                self.raw_pid()
            );
            return Err(SystemError::EPERM);
        }

        log::debug!(
            "[PTRACE_ATTACH] Setting PTRACED flag for target={}",
            self.raw_pid()
        );
        self.flags().insert(ProcessFlags::PTRACED);

        log::debug!(
            "[PTRACE_ATTACH] Calling ptrace_link: tracer={}, target={}",
            tracer.raw_pid(),
            self.raw_pid()
        );
        self.ptrace_link(tracer)?;

        // 注意：不要修改 exit_signal！
        // exit_signal 是用来表示进程退出时发送给父进程的信号（通常是 SIGCHLD）
        // ptrace 注入的信号应该存储在 ptrace_state.injected_signal 中

        let sig = Signal::SIGSTOP;
        log::debug!(
            "[PTRACE_ATTACH] Sending SIGSTOP to target={}",
            self.raw_pid()
        );
        let mut info = SigInfo::new(
            sig,
            0,
            SigCode::Origin(OriginCode::Kernel),
            SigType::SigFault(SigFaultInfo { addr: 0, trapno: 0 }),
        );
        if let Err(e) = sig.send_signal_info_to_pcb(
            Some(&mut info),
            self.self_ref.upgrade().unwrap(),
            PidType::PID,
        ) {
            log::debug!(
                "[PTRACE_ATTACH] Failed to send SIGSTOP to target={}, error={:?}",
                self.raw_pid(),
                e
            );
            // 回滚ptrace设置
            self.flags().remove(ProcessFlags::PTRACED);
            let _ = self.ptrace_unlink()?;
            return Err(e);
        }

        // 按照 Linux 6.6.21 的 ptrace_attach 语义：
        // 发送 SIGSTOP 信号后，不需要手动 kick tracee
        // tracee 会在下一次返回用户态时自动调用 do_signal 处理
        // 然后调用 ptrace_stop 并进入 TracedStopped 状态
        //
        // 关键：不要调用 kick()！如果调用 kick()，会导致 tracee 被 wake up
        // 然后 tracee 调用 schedule() 后可能立即被调度，无法正确进入 TracedStopped 状态
        log::debug!(
            "[PTRACE_ATTACH] Sent SIGSTOP to target={}, waiting for it to process signal",
            self.raw_pid()
        );

        // 等待 tracee 进入 TracedStopped 状态
        // 按照 Linux 6.6.21 语义：notify_tracer 调用 __wake_up_parent 唤醒 tracer 的 wait_queue
        // 所以这里应该等待在 tracer 的 wait_queue 上，而不是 tracee 的 wait_queue
        let tracee_ref = self.self_ref.upgrade().unwrap();
        let tracer_clone = tracer.clone();
        log::debug!(
            "[PTRACE_ATTACH] Tracer {} entering wait_event_interruptible for tracee {}",
            tracer.raw_pid(),
            tracee_ref.raw_pid()
        );
        let _wait_result = tracer_clone.wait_queue.wait_event_interruptible(
            || {
                let state = tracee_ref.sched_info().inner_lock_read_irqsave().state();
                let is_stopped = matches!(state, ProcessState::TracedStopped(_));
                log::debug!(
                    "[PTRACE_ATTACH] Wait check: tracee {} state={:?}, is_stopped={}",
                    tracee_ref.raw_pid(),
                    state,
                    is_stopped
                );
                is_stopped
            },
            None::<fn()>,
        );

        log::debug!(
            "[PTRACE_ATTACH] Attach completed: tracer={}, target={}",
            tracer.raw_pid(),
            self.raw_pid()
        );

        Ok(0)
    }

    /// 处理PTRACE_DETACH请求
    pub fn detach(&self, signal: Option<Signal>) -> Result<isize, SystemError> {
        log::debug!(
            "[PTRACE_DETACH] Detaching from PID {}, signal={:?}",
            self.raw_pid(),
            signal
        );

        // 验证调用者是跟踪器
        let current_pcb = ProcessManager::current_pcb();
        if !self.is_traced_by(&current_pcb) {
            return Err(SystemError::EPERM);
        }

        // 按照 Linux 6.6.21 的 ptrace_detach 实现：
        // 1. 将 data 参数存储到 ptrace_state.injected_signal
        // 2. 调用 __ptrace_detach (ptrace_unlink)
        // 3. 恢复进程执行（使用与 ptrace_resume 相同的唤醒逻辑）
        //
        // 关键：data 参数会被传递给 ptrace_stop，作为恢复后要处理的信号
        // 如果 data 是 0，表示不发送任何信号，继续执行
        let data_signal = signal.unwrap_or(Signal::SIGCONT);

        // 将注入的信号存储到 ptrace_state.injected_signal
        // 而不是 exit_signal！exit_signal 是用来表示进程退出时发送给父进程的信号
        let mut ptrace_state = self.ptrace_state.lock();
        ptrace_state.injected_signal = data_signal;
        drop(ptrace_state);

        log::debug!(
            "[PTRACE_DETACH] Set ptrace_state.injected_signal={:?} for PID {}",
            data_signal,
            self.raw_pid()
        );

        // 解除跟踪关系
        // ptrace_unlink 会自动恢复父进程为 real_parent
        // 并清除 ProcessFlags::PTRACED 和 TRACE_SYSCALL
        self.ptrace_unlink()?;

        // 按照 Linux 6.6.21 的 ptrace_detach 实现：
        // 使用与 ptrace_resume/wakeup_stop 相同的唤醒逻辑
        // Linux: child->jobctl &= ~JOBCTL_TRACED; wake_up_state(child, __TASK_TRACED);
        //
        // 关键：需要手动将 TracedStopped 状态改为 Runnable 并加入运行队列
        // signal_wake_up 不会唤醒 TracedStopped 状态的任务（它只唤醒 Blocked 状态）
        let mut sched_info = self.sched_info.inner_lock_write_irqsave();

        // 检查当前状态
        match sched_info.state() {
            ProcessState::TracedStopped(_) | ProcessState::Stopped(_) => {
                // 将状态设置为 Runnable，让进程可以被调度
                sched_info.set_state(ProcessState::Runnable);
                sched_info.set_wakeup();
                log::debug!(
                    "[PTRACE_DETACH] Set PID {} to Runnable and wakeup",
                    self.raw_pid()
                );
            }
            _ => {
                // 进程可能已经由于其他原因被唤醒，仍然需要确保 sleep 标志被清除
                sched_info.set_wakeup();
                log::debug!(
                    "[PTRACE_DETACH] PID {} already runnable, just wakeup",
                    self.raw_pid()
                );
            }
        }
        drop(sched_info);

        // 按照 wakeup_stop 的模式，使用 activate_task + check_preempt_currnet
        // 确保 on_rq 被正确设置为 Queued
        let rq = crate::sched::cpu_rq(
            self.sched_info()
                .on_cpu()
                .unwrap_or(crate::smp::core::smp_get_processor_id())
                .data() as usize,
        );

        let (rq, _guard) = rq.self_lock();
        rq.update_rq_clock();
        let strong_ref = self.self_ref.upgrade().unwrap();
        rq.activate_task(
            &strong_ref,
            EnqueueFlag::ENQUEUE_WAKEUP | EnqueueFlag::ENQUEUE_NOCLOCK,
        );

        rq.check_preempt_currnet(&strong_ref, WakeupFlags::empty());

        log::debug!(
            "[PTRACE_DETACH] Detached from PID {}, injected {:?}, activated and preempt-checked",
            self.raw_pid(),
            data_signal
        );

        Ok(0)
    }

    /// 恢复进程执行
    pub fn ptrace_resume(
        &self,
        request: PtraceRequest,
        signal: Option<Signal>,
        frame: &mut TrapFrame,
    ) -> Result<isize, SystemError> {
        match request {
            PtraceRequest::PtraceSyscall => self.flags().insert(ProcessFlags::TRACE_SYSCALL),
            PtraceRequest::PtraceSinglestep => {
                self.flags().insert(ProcessFlags::TRACE_SINGLESTEP);
                kprobe::setup_single_step(frame, frame.rip as usize); // 架构相关的操作，设置 TF 标志
            }
            _ => {} // PTRACE_CONT 不需要特殊标志
        }

        // 按照 Linux 6.6.21 的 ptrace_resume 语义：
        // data 参数应该存储到 ptrace_state.injected_signal
        // 如果 data=0（signal 为 None），表示不注入任何信号，tracee 继续执行
        // 如果 data != 0，表示注入指定的信号
        let resume_signal = signal.unwrap_or(Signal::INVALID);
        log::info!("signal: {:?} to process {}", resume_signal, self.raw_pid());

        // 清除停止/阻塞标志
        self.flags().remove(ProcessFlags::STOPPED);

        // 将注入的信号存储到 ptrace_state.injected_signal
        // 而不是 exit_signal！exit_signal 是用来表示进程退出时发送给父进程的信号
        let mut ptrace_state = self.ptrace_state.lock();
        ptrace_state.injected_signal = resume_signal;
        drop(ptrace_state);

        log::debug!(
            "[PTRACE_RESUME] Set ptrace_state.injected_signal={:?} for PID {}",
            resume_signal,
            self.raw_pid()
        );

        // 按照 Linux 6.6.21 的 ptrace_resume 语义：
        // 需要将 TracedStopped 状态的进程设置为 Runnable 并加入运行队列
        //
        // 注意：必须在唤醒之前设置状态，否则 tracee 可能继续睡眠
        let mut sched_info = self.sched_info.inner_lock_write_irqsave();

        // 检查当前状态
        match sched_info.state() {
            ProcessState::TracedStopped(_) | ProcessState::Stopped(_) => {
                // 将状态设置为 Runnable，让进程可以被调度
                sched_info.set_state(ProcessState::Runnable);
                sched_info.set_wakeup();
                log::debug!(
                    "[PTRACE_RESUME] Set PID {} to Runnable and wakeup",
                    self.raw_pid()
                );
            }
            _ => {
                // 进程可能已经由于其他原因被唤醒，仍然需要确保 sleep 标志被清除
                sched_info.set_wakeup();
                log::debug!(
                    "[PTRACE_RESUME] PID {} already runnable, just wakeup",
                    self.raw_pid()
                );
            }
        }
        drop(sched_info);

        // 加入调度队列（如果不在队列中的话）
        // enqueue_task 会检查是否已在队列中，避免重复加入
        if let Some(strong_ref) = self.self_ref.upgrade() {
            let rq = self.sched_info.sched_entity().cfs_rq().rq();
            let (rq, _guard) = rq.self_lock();
            rq.enqueue_task(
                strong_ref.clone(),
                EnqueueFlag::ENQUEUE_RESTORE | EnqueueFlag::ENQUEUE_WAKEUP,
            );
        } else {
            log::warn!("ptrace_resume: pid={} self_ref is dead", self.raw_pid());
        }

        Ok(0)
    }

    // 处理PTRACE_SYSCALL请求
    pub fn trace_syscall(&self) -> Result<isize, SystemError> {
        // 设置系统调用跟踪标志
        self.flags().insert(ProcessFlags::TRACE_SYSCALL);
        self.flags().remove(ProcessFlags::TRACE_SINGLESTEP);
        // 恢复进程运行
        let mut sched_info = self.sched_info.inner_lock_write_irqsave();
        match sched_info.state() {
            ProcessState::TracedStopped(_) | ProcessState::Stopped(_) => {
                sched_info.set_state(ProcessState::Runnable);
                sched_info.set_wakeup();
            }
            _ => {
                sched_info.set_wakeup();
            }
        }
        drop(sched_info);

        // 加入调度队列
        if let Some(strong_ref) = self.self_ref.upgrade() {
            let rq = self.sched_info.sched_entity().cfs_rq().rq();
            let (rq, _guard) = rq.self_lock();
            rq.enqueue_task(
                strong_ref.clone(),
                EnqueueFlag::ENQUEUE_RESTORE | EnqueueFlag::ENQUEUE_WAKEUP,
            );
        }

        Ok(0)
    }

    pub fn ptrace_get_syscall_info(
        &self,
        user_size: usize,
        datavp: usize, // Use a raw byte pointer for flexibility
    ) -> Result<isize, SystemError> {
        // todo let trap_frame = self.task_context();
        let trap_frame = TrapFrame::new();
        let ctx = kprobe::KProbeContext::from(&trap_frame);
        let mut info = PtraceSyscallInfo {
            op: PtraceSyscallInfoOp::None,
            pad: [0; 3],
            arch: kprobe::syscall_get_arch(),
            instruction_pointer: kprobe::instruction_pointer(&ctx),
            stack_pointer: kprobe::user_stack_pointer(&ctx),
            data: PtraceSyscallInfoData {
                _uninit: MaybeUninit::uninit(),
            },
        };

        let ptrace_state = self.ptrace_state.lock();
        let actual_size = match ptrace_state.stop_reason {
            PtraceStopReason::SyscallEntry => {
                info.op = PtraceSyscallInfoOp::Entry;
                let mut args = [0u64; 6];
                kprobe::syscall_get_arguments(&ctx, &mut args);
                info.data.entry = PtraceSyscallInfoEntry {
                    nr: kprobe::syscall_get_nr(&ctx),
                    args,
                };
                core::mem::size_of::<PtraceSyscallInfo>()
            }
            PtraceStopReason::SyscallExit => {
                info.op = PtraceSyscallInfoOp::Exit;
                let rval = kprobe::syscall_get_return_value(&ctx);
                let is_error = rval >= -4095; // MAX_ERRNO
                info.data.exit = PtraceSyscallInfoExit {
                    rval,
                    is_error: is_error as u8,
                };
                core::mem::size_of::<PtraceSyscallInfo>()
            }
            _ => {
                // 如果因为其他原因停止，只返回通用头部信息的大小
                core::mem::offset_of!(PtraceSyscallInfo, data)
            }
        };
        drop(ptrace_state);

        // 将数据拷贝到用户空间
        let write_size = core::cmp::min(actual_size, user_size);
        if write_size > 0 {
            // 将结构体视为字节切片进行拷贝
            let info_bytes =
                unsafe { core::slice::from_raw_parts(&info as *const _ as *const u8, write_size) };
            // datavp.write_bytes(info_bytes)?;
        }

        // 无论拷贝多少，都返回内核准备好的完整数据大小
        Ok(actual_size as isize)
    }

    /// 处理PTRACE_SINGLESTEP请求
    pub fn single_step(&self) -> Result<isize, SystemError> {
        // 设置单步执行标志
        self.flags().insert(ProcessFlags::TRACE_SINGLESTEP);
        self.flags().remove(ProcessFlags::TRACE_SYSCALL);

        // 在CPU层面启用单步执行
        // if let Some(context) = self.context_mut() {
        //     context.enable_single_step();
        // }

        // 恢复进程运行
        let mut sched_info = self.sched_info.inner_lock_write_irqsave();
        match sched_info.state() {
            ProcessState::TracedStopped(_) | ProcessState::Stopped(_) => {
                sched_info.set_state(ProcessState::Runnable);
                sched_info.set_wakeup();
            }
            _ => {
                sched_info.set_wakeup();
            }
        }
        drop(sched_info);

        // 加入调度队列
        if let Some(strong_ref) = self.self_ref.upgrade() {
            let rq = self.sched_info.sched_entity().cfs_rq().rq();
            let (rq, _guard) = rq.self_lock();
            rq.enqueue_task(
                strong_ref.clone(),
                EnqueueFlag::ENQUEUE_RESTORE | EnqueueFlag::ENQUEUE_WAKEUP,
            );
        }

        Ok(0)
    }

    /// 启用单步执行
    pub fn enable_single_step(&self) {
        // 实际实现中需要设置CPU标志
    }

    /// 启用系统调用跟踪
    pub fn enable_syscall_tracing(&self) {
        self.flags().insert(ProcessFlags::TRACE_SYSCALL);
    }

    /// 在系统调用入口处理
    pub fn on_syscall_entry(&self, num: usize, args: &[usize]) {
        // 实际实现中需要记录系统调用信息
    }

    /// 在系统调用出口处理
    pub fn on_syscall_exit(&self, result: isize) {
        // 实际实现中需要记录系统调用结果
    }

    /// 处理 PTRACE_PEEKUSER 请求
    pub fn peek_user(&self, addr: usize) -> Result<isize, SystemError> {
        // // 验证地址是否在用户空间范围内
        // if !self.memory.is_user_address(addr) {
        //     return Err(SystemError::EFAULT);
        // }
        // // 使用正确的寄存器偏移量
        // let offset = syscall_number_offset();
        // let reg_addr = offset * core::mem::size_of::<usize>();
        // // 确保访问的是寄存器区域
        // if addr != reg_addr {
        //     return Err(SystemError::EFAULT);
        // }
        // // 获取当前线程的寄存器值
        // let thread = self.current_thread().ok_or(SystemError::ESRCH)?;
        // let regs = thread.get_registers();
        // // 返回系统调用号
        // Ok(regs.syscall_number() as isize)
        Ok(0)
    }

    /// 设置PTRACE选项
    pub fn set_ptrace_options(&self, options: PtraceOptions) -> Result<(), SystemError> {
        let mut state = self.ptrace_state.lock();
        state.options = options;
        Ok(())
    }

    /// 清空待处理信号
    pub fn clear_ptrace(&self) {
        let mut ptrace_state = self.ptrace_state.lock();

        // 清除跟踪关系
        ptrace_state.tracer = None;
        // ptrace_state.siginfo = None;
        ptrace_state.pending_signals = Vec::new();
        // ptrace_state.signal_queue.clear();

        // 清除标志位
        self.flags().remove(
            ProcessFlags::PTRACED | ProcessFlags::TRACE_SYSCALL | ProcessFlags::TRACE_SINGLESTEP,
        );
    }

    fn decode_exit_code_for_siginfo(exit_code: i32) -> (SigCode, i32) {
        if (exit_code & 0x7f) == 0 {
            // 正常退出: exit()
            let status = (exit_code >> 8) & 0xff;
            (SigCode::SigChld(ChldCode::Exited), status)
        } else {
            // 因信号终止
            let signal_num = exit_code & 0x7f;
            if (exit_code & 0x80) != 0 {
                // 生成了 core dump
                (SigCode::SigChld(ChldCode::Dumped), signal_num)
            } else {
                // 未生成 core dump
                (SigCode::SigChld(ChldCode::Killed), signal_num)
            }
        }
    }
}
