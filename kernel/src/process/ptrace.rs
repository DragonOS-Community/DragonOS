use crate::arch::interrupt::TrapFrame;
use crate::arch::ipc::signal::{SigFlags, Signal};
use crate::arch::kprobe;
use crate::ipc::signal_types::{
    ChldCode, OriginCode, SigChldInfo, SigCode, SigFaultInfo, SigInfo, SigType, TrapCode,
};
use crate::process::cred;
use crate::process::{
    pid::PidType, ProcessControlBlock, ProcessFlags, ProcessManager, ProcessState, PtraceEvent,
    PtraceOptions, PtraceRequest, PtraceStopReason, PtraceSyscallInfo, PtraceSyscallInfoData,
    PtraceSyscallInfoEntry, PtraceSyscallInfoExit, PtraceSyscallInfoOp, RawPid,
};
use crate::sched::{schedule, EnqueueFlag, SchedMode, WakeupFlags};
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
    // Clone the Arc before calling ptrace_stop to prevent use-after-free.
    let pcb_clone = Arc::clone(pcb);
    // todo pcb.jobctl_set(JobControlFlags::STOP_DEQUEUED);
    // 注意：ptrace_stop 内部会处理锁的释放和重新获取。
    let signr = pcb_clone.ptrace_stop(original_signal as usize, ChldCode::Trapped, info.as_mut());

    if signr == 0 {
        return None; // 丢弃原始信号，继续处理下一个信号（如果没有，则继续执行）
    }

    // 将注入的信号转换为 Signal 类型
    let injected_signal = Signal::from(signr);
    if injected_signal == Signal::INVALID {
        return None;
    }

    // 如果追踪者注入了不同于原始信号的新信号，更新 siginfo
    if injected_signal != original_signal {
        if let Some(info_ref) = info {
            // 如果获取失败，保持原有的 siginfo
            if let Some(tracer) = pcb_clone.tracer().and_then(ProcessManager::find) {
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
            // 如果获取 tracer 失败，info 保持原样，这不是致命错误
        }
    }

    // 特殊处理 SIGCONT：需要清除挂起的停止信号，但仍然要唤醒进程并传递给用户空间处理
    if injected_signal == Signal::SIGCONT {
        // 清除任何挂起的停止信号（如 SIGSTOP, SIGTSTP 等）
        let mut sig_info = pcb_clone.sig_info.write();
        let pending = sig_info.sig_pending_mut().signal_mut();
        for stop_sig in [
            Signal::SIGSTOP,
            Signal::SIGTSTP,
            Signal::SIGTTIN,
            Signal::SIGTTOU,
        ] {
            pending.remove(stop_sig.into());
        }
        drop(sig_info);
        return Some(injected_signal);
    }

    // 检查新信号是否被当前进程的信号掩码阻塞
    let sig_set = {
        let guard = pcb_clone.sig_info_irqsave();
        *guard.sig_blocked()
    };

    if sig_set.contains(injected_signal.into()) {
        // 如果信号被阻塞了，则尝试重新入队
        match injected_signal.send_signal_info_to_pcb(info.as_mut(), pcb_clone, PidType::PID) {
            Ok(_) => return None, // 成功入队
            Err(e) => {
                // 严重错误：无法保留被阻塞的信号。
                log::error!(
                    "ptrace_signal lost signal {:?} due to re-queue failure: {:?}",
                    injected_signal,
                    e
                );
                return None;
            }
        }
    }
    // 如果没有被阻塞，则返回这个新信号，让 get_signal 继续分发和处理它。
    Some(injected_signal)
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
        self.ptrace_state.lock().tracer
    }

    pub fn is_traced(&self) -> bool {
        self.ptrace_state.lock().tracer.is_some()
    }

    pub fn is_traced_by(&self, tracer: &Arc<ProcessControlBlock>) -> bool {
        let state = self.ptrace_state.lock();
        match state.tracer {
            Some(pid) => pid == tracer.raw_pid(),
            None => false,
        }
    }

    pub fn set_state(&self, state: ProcessState) {
        let mut sched_info = self.sched_info.inner_lock_write_irqsave();
        sched_info.set_state(state);
    }

    /// 设置父进程（用于 ptrace_link 和 ptrace_unlink）
    pub fn set_parent(&self, new_parent: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        if new_parent.raw_pid() == self.raw_pid() {
            return Err(SystemError::EINVAL);
        }
        if new_parent.is_exited() {
            return Err(SystemError::ESRCH);
        }

        *(self.parent_pcb.write()) = Arc::downgrade(new_parent);
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

    fn ptrace_do_notify(
        signal: Signal,
        exit_code: usize,
        _reason: Option<i32>,
    ) -> Result<(), SystemError> {
        let current_pcb = ProcessManager::current_pcb();

        // 构造 Raw code (si_code = exit_code & 0xff)
        // Linux 中 ptrace_notify 使用 (exit_code & 0xff) 作为 si_code
        // 通常是 SIGTRAP | (PTRACE_EVENT_xxx << 8)
        let si_code = (exit_code >> 8) as i32;

        // 如果是标准的 TRAP_* 代码，使用 TrapCode
        let code = match si_code {
            1 => SigCode::Trap(TrapCode::Brkpt),
            2 => SigCode::Trap(TrapCode::Trace),
            3 => SigCode::Trap(TrapCode::Branch),
            4 => SigCode::Trap(TrapCode::Hwbkpt),
            5 => SigCode::Trap(TrapCode::Unk),
            6 => SigCode::Trap(TrapCode::Perf),
            _ => SigCode::Raw(si_code),
        };

        let mut info = SigInfo::new(
            signal, // si_signo = SIGTRAP
            0,      // si_errno = 0
            code,
            SigType::SigFault(SigFaultInfo {
                addr: 0,
                trapno: exit_code as i32, // trapno 暂时用来存完整 exit_code
            }),
        );
        current_pcb.ptrace_stop(exit_code, ChldCode::Trapped, Some(&mut info));
        Ok(())
    }

    /// ptrace 事件通知
    ///
    /// - 如果事件被启用（通过 PTRACE_O_TRACEEXEC 等选项），调用 ptrace_event 阻塞进程
    /// - 进程保持 TracedStopped 状态，直到 tracer 唤醒它
    /// - 不应该手动设置 Runnable 状态，这由 ptrace_resume 处理
    ///
    /// Legacy Exec 行为（PTRACE_SEIZE）：
    /// - 如果进程是通过 PTRACE_SEIZE 附加的（PT_SEIZED 标志已设置），
    ///   且没有设置 PTRACE_O_TRACEEXEC，则不发送 Legacy SIGTRAP
    pub fn ptrace_event(&self, event: PtraceEvent, message: usize) {
        // 检查是否启用了该事件的追踪
        if unlikely(self.ptrace_event_enabled(event)) {
            self.ptrace_state.lock().event_message = message;
            // ptrace_notify 会调用 ptrace_stop，阻塞进程直到 tracer 唤醒
            let exit_code = (event as usize) << 8 | Signal::SIGTRAP as usize;
            if let Err(e) = Self::ptrace_notify(exit_code) {
                log::error!(
                    "ptrace_event: failed to notify tracer of event {:?}: {:?}",
                    event,
                    e
                );
            }
            // ptrace_stop 内部会调用 schedule() 阻塞
            // 当 tracer 调用 PTRACE_CONT 时，ptrace_resume 会设置 Runnable
        } else if event == PtraceEvent::Exec {
            // Legacy Exec 行为：只有在非 PTRACE_SEIZE 时才发送自动 SIGTRAP
            // - PTRACE_ATTACH：发送 Legacy SIGTRAP
            // - PTRACE_SEIZE：不发送 Legacy SIGTRAP（除非显式设置 PTRACE_O_TRACEEXEC）
            let flags = self.flags();
            if flags.contains(ProcessFlags::PTRACED) && !flags.contains(ProcessFlags::PT_SEIZED) {
                // 非 PTRACE_SEIZE：发送 Legacy SIGTRAP
                let sig = Signal::SIGTRAP;
                let mut info = SigInfo::new(
                    sig,
                    0,
                    SigCode::Origin(OriginCode::Kernel),
                    SigType::SigFault(SigFaultInfo { addr: 0, trapno: 0 }),
                );
                // 如果 self_ref 升级失败，说明进程正在销毁，此时发送信号没有意义，安全地跳过
                if let Some(strong_ref) = self.self_ref.upgrade() {
                    if let Err(e) =
                        sig.send_signal_info_to_pcb(Some(&mut info), strong_ref, PidType::PID)
                    {
                        log::error!(
                            "ptrace_event: failed to send legacy SIGTRAP for exec: {:?}",
                            e
                        );
                    }
                }
            }
            // 未PTRACED或PTRACE_SEIZE：不发送信号，静默返回
        }
    }

    /// 检查是否启用了指定的 ptrace 事件选项
    ///
    /// - 检查 PTRACE_O_TRACEEXEC 等选项是否被设置
    /// - 返回 true 表示 tracer 想要接收该事件的通知
    pub fn ptrace_event_enabled(&self, event: PtraceEvent) -> bool {
        // 将 PtraceEvent 转换为对应的 PtraceOptions 标志
        let event_flag = match event {
            PtraceEvent::Fork => PtraceOptions::TRACEFORK,
            PtraceEvent::VFork => PtraceOptions::TRACEVFORK,
            PtraceEvent::Clone => PtraceOptions::TRACECLONE,
            PtraceEvent::Exec => PtraceOptions::TRACEEXEC,
            PtraceEvent::VForkDone => PtraceOptions::TRACEVFORKDONE,
            PtraceEvent::Exit => PtraceOptions::TRACEEXIT,
            PtraceEvent::Seccomp => PtraceOptions::TRACESECCOMP,
            _ => return false,
        };

        // 检查该选项是否在 ptrace_state.options 中被设置
        self.ptrace_state.lock().options.contains(event_flag)
    }

    /// 设置进程为停止状态
    ///
    /// - 设置状态为 TracedStopped (类似 TASK_TRACED)
    /// - 存储 last_siginfo（供 PTRACE_GETSIGINFO 读取）
    /// - 调用 schedule() 让出 CPU，调度器会自动将任务从运行队列移除
    pub fn ptrace_stop(
        &self,
        exit_code: usize,
        why: ChldCode,
        info: Option<&mut SigInfo>,
    ) -> usize {
        // 设置 TRAPPING 标志，表示正在停止
        self.flags().insert(ProcessFlags::TRAPPING);
        let mut sched_info = self.sched_info.inner_lock_write_irqsave();
        sched_info.set_state(ProcessState::TracedStopped(exit_code));
        sched_info.set_sleep();
        drop(sched_info);

        // 清除 ptrace_state 中的 event_message
        self.ptrace_state.lock().event_message = 0;

        // 存储 last_siginfo
        if let Some(info) = info {
            self.ptrace_state.lock().set_last_siginfo(*info);
        }

        // 通知跟踪器
        if let Some(tracer) = self.parent_pcb() {
            self.notify_tracer(&tracer, why);
        }

        // 清除 TRAPPING 标志，表示已经完成停止准备工作
        self.flags().remove(ProcessFlags::TRAPPING);

        schedule(SchedMode::SM_NONE);

        // 从 schedule() 返回后，tracer 已经通过 ptrace_resume 唤醒了我们
        // 进程恢复后，应该返回 tracer 注入的信号（data 参数）
        let mut ptrace_state = self.ptrace_state.lock();
        let injected_signal = ptrace_state.injected_signal;

        // 如果注入的信号是 INVALID，返回 0，表示没有注入信号
        let result = if injected_signal == Signal::INVALID {
            0
        } else {
            ptrace_state.injected_signal = Signal::INVALID;
            injected_signal as usize
        };
        drop(ptrace_state);

        result
    }

    fn notify_tracer(&self, tracer: &Arc<ProcessControlBlock>, why: ChldCode) {
        let status = match why {
            ChldCode::Stopped => self.exit_code().unwrap_or(0) as i32 & 0x7f,
            ChldCode::Trapped => self.exit_code().unwrap_or(0) as i32 & 0x7f,
            _ => Signal::SIGCONT as i32,
        };

        // 发送 SIGCHLD 通知父进程（tracer）
        // 这与 tracee 内部的 SIGTRAP siginfo 是分离的
        let mut chld_info = SigInfo::new(
            Signal::SIGCHLD,
            0,
            SigCode::SigChld(why),
            SigType::SigChld(SigChldInfo {
                pid: self.raw_pid(),
                uid: self.cred().uid.data(),
                status,
                utime: 0,
                stime: 0,
            }),
        );

        let should_send = {
            let tracer_sighand = tracer.sighand();
            let sa = tracer_sighand.handler(Signal::SIGCHLD);
            let force_send = why == ChldCode::Trapped;
            if let Some(sa) = sa {
                !sa.action().is_ignore()
                    && (force_send || !sa.flags().contains(SigFlags::SA_NOCLDSTOP))
            } else {
                false
            }
        };
        if should_send {
            let _ = Signal::SIGCHLD.send_signal_info_to_pcb(
                Some(&mut chld_info),
                Arc::clone(tracer),
                PidType::TGID,
            );
        }
        // 唤醒 tracer 的 wait_queue
        tracer
            .wait_queue
            .wakeup(Some(ProcessState::TracedStopped(status as usize)));
    }

    /// 检查当前进程是否有权限跟踪目标进程
    pub fn has_permission_to_trace(&self, tracee: &Self) -> bool {
        // 1. 超级用户可以跟踪任何进程
        // if self.is_superuser() {
        //     return true;
        // }

        // 2. 同一线程组允许访问（自省）
        if self.raw_tgid() == tracee.raw_tgid() {
            return true;
        }

        // 3. 检查UID、GID是否完全匹配 (euid/suid/uid、gid 都要相同)
        let caller_cred = self.cred();
        let tracee_cred = tracee.cred();
        let uid_match = caller_cred.uid == tracee_cred.euid
            && caller_cred.uid == tracee_cred.suid
            && caller_cred.uid == tracee_cred.uid;
        let gid_match = caller_cred.gid == tracee_cred.egid
            && caller_cred.gid == tracee_cred.sgid
            && caller_cred.gid == tracee_cred.gid;
        if uid_match && gid_match && tracee.dumpable() != 0 {
            return true;
        }

        // 4. 检查CAP_SYS_PTRACE权限
        caller_cred.has_capability(cred::CAPFlags::CAP_SYS_PTRACE)
    }

    pub fn ptrace_link(&self, tracer: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        if !tracer.has_permission_to_trace(self) {
            return Err(SystemError::EPERM);
        }

        self.set_tracer(tracer.raw_pid())?;
        self.set_parent(tracer)?;

        // 如果 root 进程 attach 一个普通用户进程，该进程必须保持原有权限。
        tracer.ptraced_list.write_irqsave().push(self.raw_pid());

        Ok(())
    }

    /// 解除 ptrace 跟踪关系
    pub fn ptrace_unlink(&self) -> Result<(), SystemError> {
        // 确保当前进程确实被跟踪
        if !self.is_traced() {
            return Err(SystemError::EINVAL);
        }

        // 1. 从跟踪器的跟踪列表中移除当前进程
        if let Some(tracer) = self.parent_pcb() {
            tracer
                .ptraced_list
                .write_irqsave()
                .retain(|&pid| pid != self.raw_pid());
        }

        // 2. 恢复父进程为真实父进程
        // 如果 real_parent 已退出，则过继给 init 进程（pid=1）
        let new_parent = self
            .real_parent_pcb()
            .or_else(|| ProcessManager::find_task_by_vpid(RawPid(1)))
            .ok_or(SystemError::ESRCH)?;
        self.set_parent(&new_parent)?;

        // 3. 清除 ptrace 标志和 tracer
        self.clear_tracer();

        // 4. 清除 TRAPPING 标志：表示正在停止的同步标志
        self.flags().remove(ProcessFlags::TRAPPING);

        // 5. 检查进程是否需要进入停止状态
        // Linux: 如果组停止有效且子进程未退出，则重新设置 JOBCTL_STOP_PENDING
        let is_exiting = self.flags().contains(ProcessFlags::EXITING);
        if !is_exiting {
            // 获取当前调度状态
            let mut sched_info = self.sched_info.inner_lock_write_irqsave();
            let current_state = sched_info.state();

            match current_state {
                // 如果进程处于 TracedStopped 状态
                ProcessState::TracedStopped(_exit_code) => {
                    // Linux 逻辑：如果 detach 时进程处于 TRACED 状态
                    // 需要唤醒它，让它从 ptrace_stop 中返回
                    // 唤醒后，进程会根据 injected_signal 决定后续行为
                    sched_info.set_state(ProcessState::Runnable);
                    sched_info.set_wakeup();
                    drop(sched_info);

                    // 加入运行队列，确保进程能被调度
                    if let Some(strong_ref) = self.self_ref.upgrade() {
                        let rq = crate::sched::cpu_rq(
                            self.sched_info()
                                .on_cpu()
                                .unwrap_or(crate::smp::core::smp_get_processor_id())
                                .data() as usize,
                        );
                        let (rq, _guard) = rq.self_lock();
                        rq.update_rq_clock();
                        rq.activate_task(
                            &strong_ref,
                            EnqueueFlag::ENQUEUE_WAKEUP | EnqueueFlag::ENQUEUE_NOCLOCK,
                        );
                    }
                }
                _ => {
                    // 其他状态，清除 TRAPPING 标志即可
                    drop(sched_info);
                }
            }
        }
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
        Ok(0)
    }

    /// 处理PTRACE_ATTACH请求
    pub fn attach(&self, tracer: &Arc<ProcessControlBlock>) -> Result<isize, SystemError> {
        // 验证权限
        let _is_same_process = tracer.raw_pid() == self.raw_pid();
        let is_same_thread_group = tracer.raw_tgid() == self.raw_tgid();

        if !tracer.has_permission_to_trace(self)
            || self.flags().contains(ProcessFlags::KTHREAD)
            || is_same_thread_group
        {
            return Err(SystemError::EPERM);
        }

        self.flags().insert(ProcessFlags::PTRACED);
        self.ptrace_link(tracer)?;

        // ptrace_attach 发送 SIGSTOP 作为内核信号
        let sig = Signal::SIGSTOP;
        let mut info = SigInfo::new(
            sig,
            0,
            SigCode::Origin(OriginCode::Kernel),
            SigType::Kill {
                pid: RawPid(0), // 内核发送
                uid: 0,
            },
        );
        if let Some(strong_ref) = self.self_ref.upgrade() {
            if let Err(e) = sig.send_signal_info_to_pcb(Some(&mut info), strong_ref, PidType::PID) {
                // 回滚 ptrace 设置
                self.flags().remove(ProcessFlags::PTRACED);
                self.ptrace_unlink()?;
                return Err(e);
            }
        } else {
            // 如果 self_ref 升级失败，说明进程正在销毁，回滚 ptrace 设置
            self.flags().remove(ProcessFlags::PTRACED);
            self.ptrace_unlink()?;
            return Err(SystemError::ESRCH);
        }
        // PTRACE_ATTACH 发送信号后立即返回
        Ok(0)
    }

    /// 处理PTRACE_SEIZE请求
    ///
    /// 按照 Linux 6.6.21 的实现：
    /// - PTRACE_SEIZE 是 PTRACE_ATTACH 的现代替代品
    /// - 不会发送 SIGSTOP 给 tracee
    /// - 设置 PT_SEIZED 标志，影响后续行为（如 Legacy Exec SIGTRAP）
    /// - 如果指定了 PTRACE_O_TRACEEXEC 等选项，这些选项会生效
    pub fn seize(
        &self,
        tracer: &Arc<ProcessControlBlock>,
        options: PtraceOptions,
    ) -> Result<isize, SystemError> {
        // 验证权限
        let _is_same_process = tracer.raw_pid() == self.raw_pid();
        let is_same_thread_group = tracer.raw_tgid() == self.raw_tgid();

        if !tracer.has_permission_to_trace(self)
            || self.flags().contains(ProcessFlags::KTHREAD)
            || is_same_thread_group
        {
            return Err(SystemError::EPERM);
        }

        // 设置 PTRACED 标志
        self.flags().insert(ProcessFlags::PTRACED);

        // 设置 PT_SEIZED 标志，表示使用现代 API 附加
        self.flags().insert(ProcessFlags::PT_SEIZED);

        // 建立 ptrace 关系
        self.ptrace_link(tracer)?;

        // 设置 ptrace 选项
        let mut ptrace_state = self.ptrace_state.lock();
        ptrace_state.options = options;
        drop(ptrace_state);

        // PTRACE_SEIZE 不发送 SIGSTOP，直接返回
        Ok(0)
    }

    /// 处理PTRACE_DETACH请求
    ///
    /// 注意：Linux 不重新发送信号到 pending 队列，只设置 exit_code。
    /// 如果 tracee 在 ptrace_stop 中睡眠，醒来后会读取 exit_code 作为返回值。
    /// 如果 tracee 不在 ptrace_stop 中，设置 exit_code 无效（预期行为）。
    ///
    /// 信号处理语义：
    /// - signal = None (data=0): 表示不注入信号，子进程继续运行
    /// - signal = Some(sig): 注入指定信号给子进程处理
    pub fn detach(&self, signal: Option<Signal>) -> Result<isize, SystemError> {
        // 验证调用者是跟踪器
        let current_pcb = ProcessManager::current_pcb();

        if !self.is_traced_by(&current_pcb) {
            return Err(SystemError::EPERM);
        }

        let data_signal = match signal {
            None => Signal::INVALID, // data=0 表示不注入信号
            Some(sig) => {
                if sig == Signal::INVALID {
                    // 显式指定了无效信号（这种情况在 syscall 层已被过滤）
                    return Err(SystemError::EIO);
                }
                sig
            }
        };

        let mut ptrace_state = self.ptrace_state.lock();
        ptrace_state.injected_signal = data_signal;
        drop(ptrace_state);

        // 解除 ptrace 关系，恢复 real_parent
        self.ptrace_unlink()?;

        // 唤醒处于停止状态的进程
        let mut sched_info = self.sched_info.inner_lock_write_irqsave();
        match sched_info.state() {
            ProcessState::TracedStopped(_) | ProcessState::Stopped(_) => {
                // 将状态设置为 Runnable，让进程可以被调度
                sched_info.set_state(ProcessState::Runnable);
                sched_info.set_wakeup();
            }
            _ => {
                // 进程可能已经由于其他原因被唤醒，仍然需要确保 sleep 标志被清除
                sched_info.set_wakeup();
            }
        }
        drop(sched_info);

        // 加入调度队列
        let rq = crate::sched::cpu_rq(
            self.sched_info()
                .on_cpu()
                .unwrap_or(crate::smp::core::smp_get_processor_id())
                .data() as usize,
        );
        let (rq, _guard) = rq.self_lock();
        rq.update_rq_clock();
        let strong_ref = self.self_ref.upgrade().ok_or(SystemError::ESRCH)?;
        rq.activate_task(
            &strong_ref,
            EnqueueFlag::ENQUEUE_WAKEUP | EnqueueFlag::ENQUEUE_NOCLOCK,
        );
        rq.check_preempt_currnet(&strong_ref, WakeupFlags::empty());

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
            PtraceRequest::Syscall => self.flags().insert(ProcessFlags::TRACE_SYSCALL),
            PtraceRequest::Singlestep => {
                self.flags().insert(ProcessFlags::TRACE_SINGLESTEP);
                kprobe::setup_single_step(frame, frame.rip as usize); // 设置 TF 标志
            }
            _ => {} // PTRACE_CONT 不需要特殊标志
        }

        let resume_signal = signal.unwrap_or(Signal::INVALID);

        // 清除停止/阻塞标志
        self.flags().remove(ProcessFlags::STOPPED);

        // 将注入的信号存储到 ptrace_state.injected_signal
        let mut ptrace_state = self.ptrace_state.lock();
        ptrace_state.injected_signal = resume_signal;
        drop(ptrace_state);

        // 将 TracedStopped 状态的进程设置为 Runnable 并加入运行队列
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

        // 加入调度队列（如果不在队列中的话）
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

    // 处理PTRACE_SYSCALL请求
    pub fn trace_syscall(&self) -> Result<isize, SystemError> {
        // 设置系统调用跟踪标志
        self.flags().insert(ProcessFlags::TRACE_SYSCALL);
        self.flags().remove(ProcessFlags::TRACE_SINGLESTEP);

        // 恢复进程运行
        // 进程将在下次系统调用的入口和出口自动停止
        // （在 syscall_handler 中同步调用 ptrace_stop）
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

    /// 处理 PTRACE_GETSIGINFO 请求，获取系统调用信息
    ///
    /// # 注意
    /// **此函数当前未完全实现** - 返回的数据可能不正确
    #[allow(dead_code)]
    pub fn ptrace_get_syscall_info(
        &self,
        user_size: usize,
        _datavp: usize, // Use a raw byte pointer for flexibility
    ) -> Result<isize, SystemError> {
        // TODO: 获取实际的trapframe，而不是创建空的
        // let trap_frame = self.task_context();
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
            // TODO: 实现用户空间数据拷贝
            // 需要使用 UserBufferWriter 将 info 结构体拷贝到 _datavp
            let _info_bytes =
                unsafe { core::slice::from_raw_parts(&info as *const _ as *const u8, write_size) };
            // datavp.write_bytes(info_bytes)?;
        }

        // 无论拷贝多少，都返回内核准备好的完整数据大小
        // 注意：当前返回的大小是正确的，但数据内容是空的（因为使用TrapFrame::new()）
        Ok(actual_size as isize)
    }

    /// 处理PTRACE_SINGLESTEP请求
    /// # 未实现
    /// - CPU层面的单步执行标志设置（x86_64的EFLAGS.TF位）
    #[allow(dead_code)]
    pub fn single_step(&self) -> Result<isize, SystemError> {
        // 设置单步执行标志
        self.flags().insert(ProcessFlags::TRACE_SINGLESTEP);
        self.flags().remove(ProcessFlags::TRACE_SYSCALL);

        // TODO: 在CPU层面启用单步执行
        // 需要设置x86_64的EFLAGS.TF (Trap Flag) 位
        // 参考: Linux arch/x86/kernel/ptrace.c::user_enable_single_step()
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

    /// 启用单步执行功能
    /// # TODO
    /// 需要实现架构特定的CPU标志设置：
    /// - **x86_64**: 设置 EFLAGS.TF (Trap Flag, bit 8)
    /// - **RISC-V**: 设置 sstatus.SSTEP
    /// - **ARM64**: 设置 MDSCR_EL1.SS
    ///
    /// 参考:
    /// - https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kernel/step.c#217
    #[allow(dead_code)]
    pub fn enable_single_step(&self) {
        unimplemented!()
    }

    /// 启用系统调用跟踪
    pub fn enable_syscall_tracing(&self) {
        self.flags().insert(ProcessFlags::TRACE_SYSCALL);
    }

    /// 在系统调用入口处调用
    #[allow(dead_code)]
    pub fn on_syscall_entry(&self, _num: usize, _args: &[usize]) {
        // TODO: 记录系统调用入口信息
    }

    /// 在系统调用出口处调用
    #[allow(dead_code)]
    pub fn on_syscall_exit(&self, _result: isize) {
        // TODO: 记录系统调用出口信息
    }

    /// 处理 PTRACE_PEEKUSER 请求
    /// 在Linux中，此函数读取 tracee 的 "USER" 区域数据，主要包含：
    /// - 寄存器值（通过偏移量访问）
    /// - 特殊值如调试寄存器
    #[allow(dead_code)]
    pub fn peek_user(&self, _addr: usize) -> Result<isize, SystemError> {
        // 未实现注释掉的代码：
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
    #[allow(dead_code)]
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
}
