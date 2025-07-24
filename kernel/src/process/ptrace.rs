use crate::arch::ipc::signal::{ChldCode, OriginCode, SigCode, SigFlags, Signal};
use crate::arch::CurrentIrqArch;
use crate::exception::InterruptArch;
use crate::ipc::signal_types::{SigChldInfo, SigFaultInfo, SigInfo, SigType, SignalFlags};
use crate::process::{
    ProcessControlBlock, ProcessFlags, ProcessManager, ProcessState, PtraceOptions, PtraceRequest,
    RawPid,
};
use crate::sched::{schedule, DequeueFlag, EnqueueFlag, SchedMode};
use alloc::{sync::Arc, vec::Vec};
use core::{intrinsics::unlikely, sync::atomic::Ordering};
use system_error::SystemError;

/// ptrace 系统调用的事件类型
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PtraceEvent {
    Fork = 1,
    VFork,
    Clone,
    Exec,
    VForkDone,
    Exit,
    Seccomp,
    Stop = 128, // 信号或单步执行导致的停止
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
        let sighand_lock = parent.sig_struct_irqsave();
        let sa = &sighand_lock.handlers[Signal::SIGCHLD as usize];
        // 这里简化了 !ptrace 的检查
        if signal == Signal::SIGCHLD {
            if sa.action().is_ignore() {
                // 父进程忽略 SIGCHLD，子进程应被自动回收
                autoreap = true;
                // 并且不发送信号
                effective_signal = None;
            } else if sa.flags.contains(SigFlags::SA_NOCLDWAIT) {
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
        let _ = sig.send_signal_info_to_pcb(Some(&mut info), parent);
    }
    // 因为即使父进程忽略信号，也可能在 wait() 中阻塞，需要被唤醒以返回 -ECHILD
    child.wake_up_parent(None);
    Ok(autoreap)
}

pub fn handle_ptrace_signal_stop(current_pcb: &Arc<ProcessControlBlock>, sig: Signal) {
    let mut ptrace_state = current_pcb.ptrace_state.lock();
    // ptrace_data.stop_reason = PtraceStopReason::SignalStop(sig);
    log::debug!(
        "PID {} stopping due to ptrace on signal {:?}",
        current_pcb.raw_pid(),
        sig
    );
    ptrace_state.exit_code = sig as usize;
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

    /// 恢复进程执行
    // todo
    pub fn ptrace_resume(&self, request: PtraceRequest, sig: Signal) {
        if request == PtraceRequest::PtraceSyscall {
        } else {
        }
        if request == PtraceRequest::PtraceSyscall {
            // arch::user_enable_single_step(self);
        } else {
            // arch::user_disable_single_step(self);
        }
        let mut sched_info = self.sched_info.inner_lock_write_irqsave();
        // 清除停止/阻塞标志
        self.exit_signal.store(sig, Ordering::SeqCst);
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
    }

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
        let sighand_lock = current_pcb.sig_struct_irqsave();
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
        signal.send_signal_info(Some(&mut info), current_pcb.raw_pid())?;
        Ok(())
    }

    fn ptrace_event_enabled(&self, event: PtraceEvent) -> bool {
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
            let _ = sig.send_signal_info_to_pcb(Some(&mut info), self.self_ref.upgrade().unwrap());
            // }
        }
        let wait_status = ((event as usize) << 8) | (Signal::SIGTRAP as usize);
        self.ptrace_stop(wait_status);
        self.set_state(ProcessState::Runnable);
    }
    /// 设置进程为停止状态
    fn ptrace_stop(&self, wait_status: usize) {
        self.set_state(ProcessState::Stopped(wait_status));
        if let Some(tracer) = self.parent_pcb() {
            tracer.wait_queue.wakeup(None);
        } else {
            log::error!("PID {} is traced but has no parent tracer!", self.raw_pid());
        }
        schedule(SchedMode::SM_NONE);
    }

    /// 设置进程为停止状态
    pub fn _stop_process(&self, signal: Signal) {
        let status_code = signal.into();
        {
            let mut sched_info = self.sched_info.inner_lock_write_irqsave();
            sched_info.set_state(ProcessState::Stopped(status_code));
            // 设置进程标志
            self.flags().insert(ProcessFlags::STOPPED);
            if let Some(tracer) = self.tracer() {
                self.wake_up_parent(Some(ProcessState::Stopped(status_code)));
            } else {
                let mut info = SigInfo::new(
                    signal,
                    0,
                    SigCode::Origin(OriginCode::Kernel),
                    SigType::SigFault(SigFaultInfo { addr: 0, trapno: 0 }),
                );
                let _ = signal.send_signal_info(Some(&mut info), self.parent_pid());
            }
        }
        log::debug!("Process {} stopped by signal {:?}", self.raw_pid(), signal);
        // 唤醒父进程
        self.wake_up_parent(None);
        // 移出调度队列
        if let Some(strong_ref) = self.self_ref.upgrade() {
            let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
            {
                let rq = self.sched_info.sched_entity().cfs_rq().rq();
                let (rq, _guard) = rq.self_lock();
                rq.dequeue_task(strong_ref.clone(), DequeueFlag::DEQUEUE_SAVE);
            }
            drop(irq_guard);
        }
        schedule(SchedMode::SM_NONE);
        self.flags().remove(ProcessFlags::STOPPED);
        let signal_to_deliver = {
            let mut ptrace_state = self.ptrace_state.lock();
            ptrace_state.next_pending_signal()
        };
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
        self.ptrace_state.lock().tracer = Some(tracer.raw_pid());
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
        let _ = self.set_parent(&real_parent);
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
        let sighand_lock = self.sig_struct_irqsave();
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
        if let Err(e) =
            sig.send_signal_info_to_pcb(Some(&mut info), self.self_ref.upgrade().unwrap())
        {
            // 回滚ptrace设置
            self.flags().remove(ProcessFlags::PTRACED);
            let _ = self.ptrace_unlink()?;
            return Err(e);
        }
        // {
        //     let guard = tracer.sig_struct_irqsave();
        //     signal_wake_up(self.self_ref.upgrade().unwrap(), guard, false);
        // }
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
        let mut dead = !self.is_thread_group_leader();
        if !dead {
            let real_parent = self.real_parent_pcb().ok_or(SystemError::ESRCH)?;
            if !ProcessManager::same_thread_group(&real_parent, &self.self_ref) {
                dead = do_notify_parent(self, signal.unwrap())?; // todo
                return Ok(0);
            } else if self.sig_struct_irqsave().handlers[Signal::SIGCHLD as usize]
                .action()
                .is_ignore()
            {
                self.wake_up_parent(None);
                dead = true;
            }
        }
        Ok(0)
    }

    /// 处理 PTRACE_CONT 请求
    pub fn ptrace_cont(&self, signal: Option<Signal>) -> Result<isize, SystemError> {
        log::info!(
            "PTRACE_CONT for process {}, signal: {:?}",
            self.raw_pid(),
            signal
        );
        if signal == None {
            return Ok(0);
        }
        let mut sig = Signal::SIGCONT;
        if signal != None {
            sig = Signal::from(signal.unwrap() as i32);
        }
        // 检查当前进程是否有权限操作目标进程
        let current = ProcessManager::current_pcb();
        if self.tracer() != Some(current.raw_pid()) {
            return Err(SystemError::EPERM);
        }
        // 检查进程是否被跟踪
        if !self.flags().contains(ProcessFlags::PTRACED) {
            return Err(SystemError::ESRCH);
        }
        // 检查进程是否处于停止状态
        if !self.flags().contains(ProcessFlags::STOPPED) {
            return Err(SystemError::EINVAL);
        }
        self.ptrace_resume(PtraceRequest::PtraceCont, sig);
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
