use crate::arch::interrupt::TrapFrame;
use crate::arch::ipc::signal::{SigFlags, Signal};
use crate::arch::kprobe;
use crate::arch::CurrentIrqArch;
use crate::exception::InterruptArch;
use crate::ipc::signal_types::{
    ChldCode, OriginCode, SigChldInfo, SigCode, SigFaultInfo, SigInfo, SigType, Sigaction,
    SigactionType, SignalFlags,
};
use crate::process::pid::PidType;
use crate::process::{
    ProcessControlBlock, ProcessFlags, ProcessManager, ProcessState, PtraceEvent, PtraceOptions,
    PtraceRequest, PtraceStopReason, PtraceSyscallInfo, PtraceSyscallInfoData,
    PtraceSyscallInfoEntry, PtraceSyscallInfoExit, PtraceSyscallInfoOp, RawPid, SyscallInfo,
};
use crate::sched::{schedule, DequeueFlag, EnqueueFlag, SchedMode};
use alloc::{sync::Arc, vec::Vec};
use core::{intrinsics::unlikely, mem::MaybeUninit, sync::atomic::Ordering};
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
    // todo pcb.jobctl_set(JobControlFlags::STOP_DEQUEUED);
    // 核心：调用 ptrace_stop 使进程停止并等待追踪者。
    // ptrace_stop 会返回追踪者注入的信号。
    // 注意：ptrace_stop 内部会处理锁的释放和重新获取。
    let mut signr = pcb.ptrace_stop(original_signal as usize, ChldCode::Trapped, info.as_mut());
    let mut injected_signal = Signal::from(signr);
    if injected_signal == Signal::INVALID {
        return None;
    }
    // pcb.set_state(ProcessState::Exited(0));

    // 如果追踪者注入了不同于原始信号的新信号，更新 siginfo。
    if injected_signal != original_signal {
        if let Some(info_ref) = info {
            let tracer = pcb.parent_pcb().unwrap();
            *info_ref = SigInfo::new(
                injected_signal,
                0,
                SigCode::Origin(OriginCode::User),
                SigType::SigChld(SigChldInfo {
                    pid: tracer.raw_pid(),
                    uid: tracer.cred().uid.data(),
                    status: 0,
                    utime: 0,
                    stime: 0,
                }),
            );
        }
    }
    // 检查新信号是否被当前进程的信号掩码阻塞
    let sig_info_guard = pcb.sig_info_irqsave();
    if sig_info_guard
        .sig_blocked()
        .contains(injected_signal.into())
    {
        // 如果被阻塞了，则将信号重新排队，让它在未来被处理。
        injected_signal.send_signal_info_to_pcb(info.as_mut(), Arc::clone(pcb), PidType::PID);
        // 告诉 get_signal，当前没有需要立即处理的信号。
        return None;
    }
    // 如果没有被阻塞，则返回这个新信号，让 get_signal 继续分发和处理它。
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
        sig.send_signal_info_to_pcb(Some(&mut info), parent, PidType::PID)?;
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
    // if let Some(tracer_pid) = ptrace_state.tracer {
    //     if let Some(tracer) = ProcessManager::find(tracer_pid) {
    //         let mut info = SigInfo::new(
    //             sig,
    //             0,
    //             SigCode::Origin(OriginCode::Kernel),
    //             SigType::SigChld(SigChldInfo {
    //                 pid: current_pcb.raw_pid(),
    //                 uid: current_pcb.cred().uid.data(),
    //                 status: sig as i32,
    //                 utime: 0, // 可以根据需要填充实际值
    //                 stime: 0, // 可以根据需要填充实际值
    //             }),
    //         );
    //         let _ = Signal::SIGCHLD.send_signal_info_to_pcb(Some(&mut info), tracer);
    //     }
    // }
    current_pcb.set_state(ProcessState::TracedStopped);
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
        // self.last_siginfo = info.cloned();
        // self.set_state(ProcessState::Exited(exit_code));
        self.set_state(ProcessState::Stopped(exit_code));
        self.flags().insert(ProcessFlags::PTRACED);
        if let Some(tracer) = self.parent_pcb() {
            self.notify_tracer(&tracer, why);
        } else {
            log::error!("PID {} is traced but has no parent tracer!", self.raw_pid());
        }
        if self.preempt_count() > 0 {
            log::warn!(
                "PID {} calling schedule with preempt_count={}",
                self.raw_pid(),
                self.preempt_count()
            );
        }
        // // 先释放 sighand_lock 锁，再获取锁
        // // 不会写，先手动维护一下 preempt
        // unsafe { self.sig_struct.force_unlock() };
        // self.preempt_count.fetch_sub(1, Ordering::SeqCst);
        // schedule(SchedMode::SM_NONE);
        // self.preempt_count.fetch_add(1, Ordering::SeqCst);
        // let sighand_lock = self.sig_struct_irqsave();
        // 为下次stop恢复
        // self.last_siginfo = None;
        self.ptrace_state.lock().event_message = 0;
        exit_code
    }

    fn notify_tracer(&self, tracer: &Arc<ProcessControlBlock>, why: ChldCode) {
        log::debug!("notify_tracer");
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
                PidType::PID,
            );
        }
        tracer.wait_queue.wakeup(None);
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
        if !tracer.has_permission_to_trace(self) {
            return Err(SystemError::EPERM);
        }
        // 将子进程添加到父进程的跟踪列表
        // let mut ptrace_list = tracer.ptraced_list.write();
        // let child_pid = self.raw_pid();
        // if ptrace_list.iter().any(|&pid| pid == child_pid) {
        //     return Err(SystemError::EALREADY);
        // }
        // ptrace_list.push(child_pid);
        self.set_tracer(tracer.raw_pid())?;
        self.set_parent(tracer)?;
        *self.cred.lock() = tracer.cred().clone();
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
        Ok(0)
    }

    /// 处理PTRACE_ATTACH请求
    pub fn attach(&self, tracer: &Arc<ProcessControlBlock>) -> Result<isize, SystemError> {
        // 验证权限（简化版）
        if !tracer.has_permission_to_trace(self)
            || self.flags().contains(ProcessFlags::KTHREAD)
            || ProcessManager::same_thread_group(tracer, &self.self_ref)
        {
            return Err(SystemError::EPERM);
        }
        log::info!("attach by Tracer: {}", tracer.raw_pid());
        self.flags().insert(ProcessFlags::PTRACED);
        self.ptrace_link(tracer)?;
        let sig = Signal::SIGSTOP;
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
            // 回滚ptrace设置
            self.flags().remove(ProcessFlags::PTRACED);
            let _ = self.ptrace_unlink()?;
            return Err(e);
        }
        // {
        //     let guard = tracer.sighand();
        //     signal_wake_up(self.self_ref.upgrade().unwrap(), guard, false);
        // }
        // todo proc_ptrace_connector(self, PTRACE_ATTACH);
        // 确保目标进程被唤醒以处理 SIGSTOP
        // 如果目标正在 sleep (INTERRUPTIBLE)，kick 它让它处理信号
        ProcessManager::kick(&self.self_ref.upgrade().unwrap());
        Ok(0)
    }

    /// 处理PTRACE_DETACH请求
    pub fn detach(&self, signal: Option<Signal>) -> Result<isize, SystemError> {
        // 验证调用者是跟踪器
        let current_pcb = ProcessManager::current_pcb();
        if !self.is_traced_by(&current_pcb) {
            return Err(SystemError::EPERM);
        }
        self.ptrace_unlink()?;
        let mut sched_info = self.sched_info.inner_lock_write_irqsave();
        if let Some(sig) = signal {
            // self.exit_signal.store(sig, Ordering::SeqCst);
            self.ptrace_state.lock().exit_code = sig as usize;
        } else {
            // return Ok(0);
            self.ptrace_state.lock().exit_code = 0;
        }
        let mut dead = !self.is_thread_group_leader();
        if !dead {
            let real_parent = self.real_parent_pcb().ok_or(SystemError::ESRCH)?;
            if !ProcessManager::same_thread_group(&real_parent, &self.self_ref) {
                log::debug!("do_notify_parent, sig={:?}", signal.unwrap());
                dead = do_notify_parent(self, signal.unwrap())?;
                return Ok(0);
            } else if self
                .sighand()
                .handler(Signal::SIGCHLD)
                .unwrap_or_default()
                .action()
                .is_ignore()
            {
                // todo unwrap?
                self.wake_up_parent(None);
                dead = true;
            }
        }
        // todo
        // if dead {
        //     self.exit_state.store(EXIT_DEAD, Ordering::SeqCst);
        // }
        // todo proc_ptrace_connector(self, PtraceRequest::PtraceDetach)
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
        log::info!("signal: {:?} to process {}", signal, self.raw_pid());
        if signal == None {
            self.exit_signal.store(Signal::SIGCONT, Ordering::SeqCst);
            return Ok(0);
        }
        let mut sched_info = self.sched_info.inner_lock_write_irqsave();
        // 清除停止/阻塞标志
        if let Some(sig) = signal {
            self.exit_signal.store(sig, Ordering::SeqCst);
        }
        self.flags().remove(ProcessFlags::STOPPED);
        // 设置为可运行状态
        sched_info.set_state(ProcessState::Runnable);
        // 加入调度队列
        if let Some(strong_ref) = self.self_ref.upgrade() {
            let rq = self.sched_info.sched_entity().cfs_rq().rq();
            let (rq, _guard) = rq.self_lock();
            rq.enqueue_task(
                strong_ref.clone(),
                EnqueueFlag::ENQUEUE_RESTORE | EnqueueFlag::ENQUEUE_WAKEUP,
            );
        } else {
            log::warn!("ptrace_runnable: pid={} self_ref is dead", self.raw_pid());
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
        if let ProcessState::Stopped(_signal) = sched_info.state() {
            sched_info.set_state(ProcessState::Runnable);
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
        if let ProcessState::Stopped(_signal) = sched_info.state() {
            sched_info.set_state(ProcessState::Runnable);
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
