use alloc::{sync::Arc, vec::Vec};
use core::{
    intrinsics::unlikely,
    mem::{offset_of, size_of},
    sync::atomic::{compiler_fence, Ordering},
};
use system_error::SystemError;

use crate::{
    arch::{
        interrupt::{TrapFrame, UserRegsStruct},
        ipc::signal::{SigFlags, Signal},
        kprobe,
    },
    ipc::signal_types::{
        ChldCode, OriginCode, SigChldInfo, SigCode, SigFaultInfo, SigInfo, SigType, SignalFlags,
        TrapCode,
    },
    process::{
        cred, pid::PidType, ProcessControlBlock, ProcessFlags, ProcessManager, ProcessState, RawPid,
    },
    sched::{schedule, SchedMode},
    syscall::user_access::UserBufferWriter,
};

/// ptrace 系统调用的请求类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum PtraceRequest {
    Traceme = 0,
    Peekdata = 2,
    Peekuser = 3,
    Pokedata = 5,
    Cont = 7,
    Singlestep = 9,
    Getregs = 12,
    Setregs = 13,
    Attach = 16,
    Detach = 17,
    Syscall = 24,
    Setoptions = 0x4200,
    Getsiginfo = 0x4202,
    Getsyscallinfo = 0x420e,
    Seize = 0x4206, // 现代 API，不发送 SIGSTOP
}

/// ptrace 系统调用的事件类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

pub const PTRACE_EVENTMSG_SYSCALL_ENTRY: usize = 1;
pub const PTRACE_EVENTMSG_SYSCALL_EXIT: usize = 2;

bitflags::bitflags! {
    /// Ptrace选项（PTRACE_O_*）
    #[derive(Default)]
    pub struct PtraceOptions: usize {
        const TRACESYSGOOD   = 1 << 0;
        const TRACEFORK      = 1 << 1;
        const TRACEVFORK     = 1 << 2;
        const TRACECLONE     = 1 << 3;
        const TRACEEXEC      = 1 << 4;
        const TRACEVFORKDONE = 1 << 5;
        const TRACEEXIT      = 1 << 6;
        const TRACESECCOMP   = 1 << 7;
    }
}

#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PtraceSyscallInfoOp {
    None = 0,
    Entry = 1,
    Exit = 2,
    #[allow(dead_code)]
    Seccomp = 3,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct PtraceSyscallInfoEntry {
    pub nr: u64,
    pub args: [u64; 6],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct PtraceSyscallInfoExit {
    pub rval: i64,
    pub is_error: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct PtraceSyscallInfoSeccomp {
    pub nr: u64,
    pub args: [u64; 6],
    pub ret_data: u32,
}

#[repr(C)]
pub union PtraceSyscallInfoData {
    pub entry: PtraceSyscallInfoEntry,
    pub exit: PtraceSyscallInfoExit,
    pub seccomp: PtraceSyscallInfoSeccomp,
}

impl Default for PtraceSyscallInfoData {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}

#[inline(always)]
fn syscall_retval_is_error(retval: i64) -> bool {
    (-4095..=-1).contains(&retval)
}

#[repr(C)]
pub struct PtraceSyscallInfo {
    /// PTRACE_SYSCALL_INFO_*
    pub op: PtraceSyscallInfoOp,
    pub pad: [u8; 3],
    pub arch: u32,
    pub instruction_pointer: u64,
    pub stack_pointer: u64,
    /// The union containing event-specific data.
    pub data: PtraceSyscallInfoData,
}

impl Default for PtraceSyscallInfo {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}

impl PtraceSyscallInfo {
    /// 最底层的构造函数，传入原始数值
    pub fn new(arch: u32, ip: u64, sp: u64) -> Self {
        Self {
            op: PtraceSyscallInfoOp::None,
            pad: [0; 3],
            arch,
            instruction_pointer: ip,
            stack_pointer: sp,
            data: PtraceSyscallInfoData::default(),
        }
    }

    /// 从 Context 直接构建
    pub fn from_context(ctx: &kprobe::KProbeContext) -> Self {
        Self::new(
            kprobe::syscall_get_arch(),
            kprobe::instruction_pointer(ctx),
            kprobe::user_stack_pointer(ctx),
        )
    }

    /// 将当前 Info 转换为 Entry 状态，并填充参数
    pub fn with_entry(mut self, nr: u64, args: [u64; 6]) -> Self {
        self.op = PtraceSyscallInfoOp::Entry;
        self.data.entry = PtraceSyscallInfoEntry { nr, args };
        self
    }

    /// 将当前 Info 转换为 Exit 状态，并填充返回值
    pub fn with_exit(mut self, rval: i64, is_error: bool) -> Self {
        self.op = PtraceSyscallInfoOp::Exit;
        self.data.exit = PtraceSyscallInfoExit {
            rval,
            is_error: is_error as u8,
        };
        self
    }

    /// 将当前 Info 转换为 Seccomp 状态
    #[allow(dead_code)]
    pub fn with_seccomp(mut self, nr: u64, args: [u64; 6], ret_data: u32) -> Self {
        self.op = PtraceSyscallInfoOp::Seccomp;
        self.data.seccomp = PtraceSyscallInfoSeccomp { nr, args, ret_data };
        self
    }
}

/// 进程被跟踪的状态信息
#[derive(Debug)]
pub struct PtraceState {
    /// 跟踪此进程的进程PID
    tracer: Option<RawPid>,
    /// 挂起的信号（等待调试器处理）
    pending_signals: Vec<Signal>,
    /// ptrace选项位
    options: PtraceOptions,
    /// 用于存储事件消息
    event_message: usize,
    /// tracer 注入的信号（在 ptrace_stop 返回后要处理的信号）
    injected_signal: Signal,
    /// 最后一次 ptrace 停止时的 siginfo（供 PTRACE_GETSIGINFO 读取）
    last_siginfo: Option<SigInfo>,
}

impl Default for PtraceState {
    fn default() -> Self {
        Self {
            tracer: None,
            pending_signals: Vec::new(),
            options: PtraceOptions::empty(),
            event_message: 0,
            injected_signal: Signal::INVALID,
            last_siginfo: None,
        }
    }
}

impl PtraceState {
    /// 获取停止状态的状态字
    pub fn status_code(&self) -> usize {
        // 根据信号和状态生成状态码
        if let Some(signal) = self.pending_signals.first() {
            (*signal as usize) << 8
        } else {
            0
        }
    }

    /// 检查是否有挂起的信号
    pub fn has_pending_signals(&self) -> bool {
        !self.pending_signals.is_empty()
    }

    /// 添加挂起信号
    pub fn add_pending_signal(&mut self, signal: Signal) {
        self.pending_signals.push(signal);
    }

    /// 获取下一个挂起信号
    pub fn next_pending_signal(&mut self) -> Option<Signal> {
        if self.pending_signals.is_empty() {
            None
        } else {
            Some(self.pending_signals.remove(0))
        }
    }

    /// 获取 last_siginfo（供 PTRACE_GETSIGINFO 使用）
    pub fn last_siginfo(&self) -> Option<SigInfo> {
        self.last_siginfo
    }

    /// 设置 last_siginfo
    pub fn set_last_siginfo(&mut self, info: SigInfo) {
        self.last_siginfo = Some(info);
    }

    /// 清除 last_siginfo
    pub fn clear_last_siginfo(&mut self) {
        self.last_siginfo = None;
    }

    /// 获取 tracer PID
    pub fn tracer(&self) -> Option<RawPid> {
        self.tracer
    }

    /// 设置 tracer PID
    pub fn set_tracer(&mut self, tracer: RawPid) {
        self.tracer = Some(tracer);
    }

    /// 清除 tracer
    pub fn clear_tracer(&mut self) {
        self.tracer = None;
    }

    /// 获取 ptrace options
    pub fn options(&self) -> PtraceOptions {
        self.options
    }

    /// 设置 ptrace options
    pub fn set_options(&mut self, options: PtraceOptions) {
        self.options = options;
    }

    pub fn set_event_message(&mut self, message: usize) {
        self.event_message = message;
    }
}

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
    // SIGKILL 不经过 ptrace_signal，必须直接递送。
    if original_signal == Signal::SIGKILL {
        return Some(Signal::SIGKILL);
    }

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
            // 严格对标 Linux: 重新初始化 siginfo，来源固定为 SI_USER
            // 获取当前父进程 (在 ptrace 期间通常是 tracer)
            let parent = pcb_clone.parent_pcb();
            let pid = parent.as_ref().map_or(RawPid::new(0), |p| p.raw_pid());
            let uid = parent.as_ref().map_or(0, |p| p.cred().uid.data() as u32);
            // 相当于 Linux 的 clear_siginfo + 填充 SI_USER 字段
            *info_ref = SigInfo::new(
                injected_signal,
                0, // errno
                SigCode::Origin(OriginCode::User),
                SigType::Kill { pid, uid },
            );
        }
    }

    // 如果 tracer 注入的新信号已被当前掩码阻塞，或者当前线程已有致命 SIGKILL 挂起，
    // 都需要把这个信号重新入队并返回 None，由后续 get_signal() 迭代继续处理。
    let sig_set = {
        let guard = pcb_clone.sig_info_irqsave();
        *guard.sig_blocked()
    };

    let has_fatal_pending = Signal::fatal_signal_pending(&pcb_clone);
    if sig_set.contains(injected_signal.into()) || has_fatal_pending {
        // blocked 或 fatal_signal_pending 两种情况下都重入队，而不是直接丢弃。
        match injected_signal.send_signal_info_to_pcb(info.as_mut(), pcb_clone, PidType::PID) {
            Ok(_) => return None, // 成功入队，返回 None 表示当前不处理
            Err(e) => {
                // 严重错误：无法保留需要重入队的信号。
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
    fn tracee_trap_frame_ptr(&self) -> *mut TrapFrame {
        let kstack = self.kernel_stack();
        let trap_frame_ptr = kstack.stack_max_address().data() - size_of::<TrapFrame>();
        trap_frame_ptr as *mut TrapFrame
    }

    fn tracee_trap_frame(&self) -> &TrapFrame {
        unsafe { &*self.tracee_trap_frame_ptr().cast_const() }
    }

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
            0,
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
                            "ptrace_event: failed to send legacy SIGTRAP for exec (pid={:?}): {:?}. Tracer may hang waiting.",
                            self.raw_pid(),
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
    /// - 返回 tracer 在恢复 tracee 时指定的 signal；0 表示不注入信号
    pub fn ptrace_stop(
        &self,
        exit_code: usize,
        why: ChldCode,
        info: Option<&mut SigInfo>,
    ) -> usize {
        // 前置检查：ptrace 关系是否还存在
        if !self.is_traced() {
            log::warn!(
                "ptrace_stop: pid={:?} is not being traced, returning early",
                self.raw_pid()
            );
            return exit_code;
        }

        // 检查是否有致命信号待处理
        // 如果有致命信号（SIGKILL 等），应该立即返回，不进入 ptrace 停止
        if let Some(strong_ref) = self.self_ref.upgrade() {
            if Signal::fatal_signal_pending(&strong_ref) {
                log::debug!(
                    "ptrace_stop: pid={:?} has fatal signal pending, skipping ptrace stop",
                    self.raw_pid()
                );
                return exit_code;
            }
        }

        // 设置 TRAPPING 标志，表示正在停止
        self.flags().insert(ProcessFlags::TRAPPING);
        let mut sched_info = self.sched_info.inner_lock_write_irqsave();
        sched_info.set_state(ProcessState::TracedStopped(exit_code));
        sched_info.set_sleep();

        if let Some(info) = info {
            self.ptrace_state.lock().set_last_siginfo(*info);
        }

        // TODO: 这里应使用等价于 smp_wmb() 的体系结构内存屏障，并与
        // JOBCTL_TRAPPING/wake_up_bit 机制配套同步。
        // 当前尚未实现对应 JOBCTL_TRAPPING 机制，只保留编译器屏障。
        compiler_fence(Ordering::Release);

        // TODO: 清除 TRAPPING 时应唤醒等待 JOBCTL_TRAPPING 的 tracer。
        self.flags().remove(ProcessFlags::TRAPPING);

        drop(sched_info);

        // 通知跟踪器
        if let Some(tracer) = self.parent_pcb() {
            self.notify_tracer(&tracer, why, exit_code);
        } else {
            log::warn!(
                "ptrace_stop: pid={:?} has no parent_pcb, may be orphaned",
                self.raw_pid()
            );
        }

        schedule(SchedMode::SM_NONE);

        // event_message/last_siginfo 必须在 tracee 被 tracer 唤醒、从 schedule() 返回后再清理，
        // 不能在 notify_tracer() 之前清零，否则 tracer 的 PTRACE_GETEVENTMSG / GETSIGINFO 会读到 0 或陈旧值。
        // DragonOS 目前没有 Linux 的 exit_code/jobctl 机制，这里用 injected_signal
        // 承载 ptrace_resume(data) 的恢复信号，并在返回到调用方前清理 stop 元数据。
        let mut ptrace_state = self.ptrace_state.lock();
        let injected_signal = ptrace_state.injected_signal;
        ptrace_state.clear_last_siginfo();
        ptrace_state.event_message = 0;

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

    fn notify_tracer(&self, tracer: &Arc<ProcessControlBlock>, why: ChldCode, stop_code: usize) {
        let status = match why {
            ChldCode::Stopped | ChldCode::Trapped => (stop_code & 0x7f) as i32,
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
            if let Err(e) = Signal::SIGCHLD.send_signal_info_to_pcb(
                Some(&mut chld_info),
                Arc::clone(tracer),
                PidType::TGID,
            ) {
                log::error!(
                    "notify_tracer: failed to send SIGCHLD to tracer pid={:?}: {:?}",
                    tracer.raw_pid(),
                    e
                );
            }
        }

        // 唤醒 tracer 的 wait_queue
        // 注意：wakeup 返回 false 只是表示当前没有等待者，不是错误
        let wakeup_ok = tracer.wait_queue.wake_one();
        if !wakeup_ok {
            log::debug!(
                "notify_tracer: wait_queue.wake_one() returned false, tracer may not be waiting yet, pid={:?}",
                tracer.raw_pid()
            );
        }
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
    /// TODO: group_stop_count/JOBCTL_STOP_PENDING 尚未实现，存在已知差异。
    pub fn ptrace_unlink(&self) -> Result<(), SystemError> {
        // 确保当前进程确实被跟踪
        if !self.is_traced() {
            return Err(SystemError::EINVAL);
        }

        // 从跟踪器的跟踪列表中移除当前进程
        if let Some(tracer) = self.parent_pcb() {
            tracer
                .ptraced_list
                .write_irqsave()
                .retain(|&pid| pid != self.raw_pid());
        }

        // 恢复父进程为真实父进程
        // 如果 real_parent 已退出，则过继给 init 进程（pid=1）
        let new_parent = self
            .real_parent_pcb()
            .or_else(|| ProcessManager::find_task_by_vpid(RawPid(1)))
            .ok_or(SystemError::ESRCH)?;
        self.set_parent(&new_parent)?;

        // 清除 ptrace 标志和 tracer
        self.clear_tracer();

        // 清除 TRAPPING 标志
        self.flags().remove(ProcessFlags::TRAPPING);

        // TODO: group_stop_count 尚未实现；当前仅近似使用 STOP_STOPPED 判定 group stop。
        let is_exiting = self.flags().contains(ProcessFlags::EXITING);
        let group_stop_still_active =
            !is_exiting && self.sighand().flags_contains(SignalFlags::STOP_STOPPED);

        // 检查当前状态并提取退出码
        let (is_traced_stopped, exit_code) = {
            let sched_info = self.sched_info.inner_lock_read_irqsave();
            let state = sched_info.state();
            (
                matches!(state, ProcessState::TracedStopped(_)),
                if let ProcessState::TracedStopped(code) = state {
                    Some(code)
                } else {
                    None
                },
            )
        };

        if is_traced_stopped {
            if group_stop_still_active {
                // Group stop 仍然有效：将 TracedStopped 转换为 Stopped
                // 进程应该继续停止，只是状态从 ptrace stop 转回 group stop
                let stop_sig = exit_code.unwrap_or(Signal::SIGSTOP as usize) & 0x7f;
                let mut sched_info = self.sched_info.inner_lock_write_irqsave();
                sched_info.set_state(ProcessState::Stopped(stop_sig));
                drop(sched_info);

                log::debug!(
                    "ptrace_unlink: pid={:?} converted TracedStopped to Stopped (group stop restored)",
                    self.raw_pid()
                );
            } else {
                // Group stop 不再有效：唤醒进程
                if let Some(strong_ref) = self.self_ref.upgrade() {
                    if let Err(e) = ProcessManager::wakeup(&strong_ref) {
                        log::error!(
                            "ptrace_unlink: failed to wakeup tracee pid={:?}: {:?}",
                            self.raw_pid(),
                            e
                        );
                    }
                }
                log::debug!(
                    "ptrace_unlink: pid={:?} woke up (group stop not active)",
                    self.raw_pid()
                );
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
        self.ptrace_link(&parent)?;
        Ok(0)
    }

    /// 处理PTRACE_ATTACH请求
    /// TODO: JOBCTL_TRAPPING/wait_on_bit 同步尚未实现，存在已知差异。
    pub fn attach(&self, tracer: &Arc<ProcessControlBlock>) -> Result<isize, SystemError> {
        // 验证权限
        let is_same_thread_group = tracer.raw_tgid() == self.raw_tgid();

        if !tracer.has_permission_to_trace(self)
            || self.flags().contains(ProcessFlags::KTHREAD)
            || is_same_thread_group
        {
            return Err(SystemError::EPERM);
        }

        self.ptrace_link(tracer)?;

        // !SEIZE 的 ATTACH 总是先发送 SIGSTOP
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

        let strong_ref = if let Some(strong_ref) = self.self_ref.upgrade() {
            strong_ref
        } else {
            self.flags().remove(ProcessFlags::PTRACED);
            if let Err(rollback_err) = self.ptrace_unlink() {
                log::error!(
                    "attach: failed to rollback ptrace for pid={:?} after self_ref upgrade failure: {:?}",
                    self.raw_pid(),
                    rollback_err
                );
            }
            return Err(SystemError::ESRCH);
        };

        if let Err(e) =
            sig.send_signal_info_to_pcb(Some(&mut info), strong_ref.clone(), PidType::PID)
        {
            self.flags().remove(ProcessFlags::PTRACED);
            if let Err(rollback_err) = self.ptrace_unlink() {
                log::error!(
                    "attach: failed to rollback ptrace for pid={:?} after signal send failure: {:?}",
                    self.raw_pid(),
                    rollback_err
                );
            }
            return Err(e);
        }

        // 如果目标已处于 group-stop，触发 STOPPED -> TRACED 转换。
        let is_already_stopped = {
            let sched_info = self.sched_info.inner_lock_read_irqsave();
            matches!(sched_info.state(), ProcessState::Stopped(_))
        };
        if is_already_stopped {
            self.flags().insert(ProcessFlags::PENDING_PTRACE_STOP);
            if let Err(e) = ProcessManager::wakeup_stop(&strong_ref) {
                log::error!(
                    "attach: failed to wakeup_stop already-stopped tracee pid={:?}: {:?}",
                    self.raw_pid(),
                    e
                );
            }
        }
        // PTRACE_ATTACH 发送信号后立即返回
        Ok(0)
    }

    /// 处理PTRACE_SEIZE请求
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
        let is_same_thread_group = tracer.raw_tgid() == self.raw_tgid();

        if !tracer.has_permission_to_trace(self)
            || self.flags().contains(ProcessFlags::KTHREAD)
            || is_same_thread_group
        {
            return Err(SystemError::EPERM);
        }

        // 建立 ptrace 关系
        self.ptrace_link(tracer)?;

        // 设置 PT_SEIZED 标志，表示使用现代 API 附加。
        self.flags().insert(ProcessFlags::PT_SEIZED);

        // 设置 ptrace 选项
        let mut ptrace_state = self.ptrace_state.lock();
        ptrace_state.options = options;
        drop(ptrace_state);

        // PTRACE_SEIZE 不发送 SIGSTOP，直接返回
        Ok(0)
    }

    /// 处理PTRACE_DETACH请求
    ///
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

        // 设置注入信号
        let mut ptrace_state = self.ptrace_state.lock();
        ptrace_state.injected_signal = data_signal;
        drop(ptrace_state);

        // 解除 ptrace 关系，恢复 real_parent
        self.ptrace_unlink()?;

        Ok(0)
    }

    /// 恢复进程执行
    pub fn ptrace_resume(
        &self,
        request: PtraceRequest,
        signal: Option<Signal>,
        _frame: &mut TrapFrame,
    ) -> Result<isize, SystemError> {
        match request {
            // 对标 Linux ptrace_resume：
            // - PTRACE_SYSCALL: 开启 syscall-trace，关闭 single-step
            // - PTRACE_SINGLESTEP: 开启 single-step，关闭 syscall-trace
            // - PTRACE_CONT: 两者都关闭
            PtraceRequest::Syscall => {
                self.flags().insert(ProcessFlags::TRACE_SYSCALL);
                self.flags().remove(ProcessFlags::TRACE_SINGLESTEP);
                let tracee_frame = unsafe { &mut *self.tracee_trap_frame_ptr() };
                let ctx = kprobe::KProbeContext::from(&*tracee_frame);
                let ip = kprobe::instruction_pointer(&ctx) as usize;
                kprobe::clear_single_step(tracee_frame, ip);
            }
            PtraceRequest::Singlestep => {
                self.flags().insert(ProcessFlags::TRACE_SINGLESTEP);
                self.flags().remove(ProcessFlags::TRACE_SYSCALL);
                let tracee_frame = unsafe { &mut *self.tracee_trap_frame_ptr() };
                let ctx = kprobe::KProbeContext::from(&*tracee_frame);
                let ip = kprobe::instruction_pointer(&ctx) as usize;
                kprobe::setup_single_step(tracee_frame, ip);
            }
            PtraceRequest::Cont => {
                self.flags()
                    .remove(ProcessFlags::TRACE_SYSCALL | ProcessFlags::TRACE_SINGLESTEP);
                let tracee_frame = unsafe { &mut *self.tracee_trap_frame_ptr() };
                let ctx = kprobe::KProbeContext::from(&*tracee_frame);
                let ip = kprobe::instruction_pointer(&ctx) as usize;
                kprobe::clear_single_step(tracee_frame, ip);
            }
            _ => return Err(SystemError::EINVAL),
        }

        // ptrace_resume() 只校验 data 是否为有效信号，并把它作为
        // “resume 后交还给 tracee 的信号”传回 ptrace_stop() 调用者。
        if matches!(signal, Some(Signal::INVALID)) {
            return Err(SystemError::EIO);
        }
        let resume_signal = signal.unwrap_or(Signal::INVALID);

        // 将注入的信号存储到 ptrace_state.injected_signal
        let mut ptrace_state = self.ptrace_state.lock();
        ptrace_state.injected_signal = resume_signal;
        drop(ptrace_state);

        // 对标  wake_up_state(child, __TASK_TRACED)：这里只恢复 ptrace-stop。
        let sched_info = self.sched_info.inner_lock_read_irqsave();
        let wake_traced = matches!(sched_info.state(), ProcessState::TracedStopped(_));
        drop(sched_info);

        if wake_traced {
            if let Some(strong_ref) = self.self_ref.upgrade() {
                if let Err(e) = ProcessManager::wakeup(&strong_ref) {
                    log::error!(
                        "ptrace_resume: failed to wakeup tracee pid={:?}: {:?}",
                        self.raw_pid(),
                        e
                    );
                }
            }
        }

        Ok(0)
    }

    /// 处理 PTRACE_GET_SYSCALL_INFO 请求，获取系统调用信息
    pub fn ptrace_get_syscall_info(
        &self,
        user_size: usize,
        datavp: usize,
    ) -> Result<isize, SystemError> {
        // 对标 task_pt_regs(child)：从 tracee 当前保存的 TrapFrame 读取寄存器。
        let trap_frame = self.tracee_trap_frame();
        let ctx = kprobe::KProbeContext::from(trap_frame);
        let base_info = PtraceSyscallInfo::from_context(&ctx);

        let (last_siginfo, ptrace_message) = {
            let ptrace_state = self.ptrace_state.lock();
            (ptrace_state.last_siginfo(), ptrace_state.event_message)
        };
        let syscall_sigtrap_code = (Signal::SIGTRAP as i32) | 0x80;
        let seccomp_sigtrap_code = (Signal::SIGTRAP as i32) | ((PtraceEvent::Seccomp as i32) << 8);

        let (info, actual_size) = match last_siginfo.map(|info| i32::from(info.sig_code())) {
            Some(code)
                if code == syscall_sigtrap_code
                    && ptrace_message == PTRACE_EVENTMSG_SYSCALL_ENTRY =>
            {
                let mut args = [0u64; 6];
                kprobe::syscall_get_arguments(&ctx, &mut args);
                let nr = kprobe::syscall_get_nr(&ctx);
                (
                    base_info.with_entry(nr, args),
                    offset_of!(PtraceSyscallInfo, data) + size_of::<PtraceSyscallInfoEntry>(),
                )
            }
            Some(code)
                if code == syscall_sigtrap_code
                    && ptrace_message == PTRACE_EVENTMSG_SYSCALL_EXIT =>
            {
                let rval = kprobe::syscall_get_return_value(&ctx);
                (
                    base_info.with_exit(rval, syscall_retval_is_error(rval)),
                    offset_of!(PtraceSyscallInfo, data)
                        + offset_of!(PtraceSyscallInfoExit, is_error)
                        + size_of::<u8>(),
                )
            }
            Some(code) if code == seccomp_sigtrap_code => {
                let mut args = [0u64; 6];
                kprobe::syscall_get_arguments(&ctx, &mut args);
                let nr = kprobe::syscall_get_nr(&ctx);
                (
                    base_info.with_seccomp(nr, args, ptrace_message as u32),
                    offset_of!(PtraceSyscallInfo, data) + size_of::<PtraceSyscallInfoSeccomp>(),
                )
            }
            _ => (base_info, offset_of!(PtraceSyscallInfo, data)),
        };

        // 将数据拷贝到用户空间
        let write_size = core::cmp::min(actual_size, user_size);
        if write_size > 0 {
            let info_bytes =
                unsafe { core::slice::from_raw_parts(&info as *const _ as *const u8, write_size) };
            let mut writer = UserBufferWriter::new(datavp as *mut u8, write_size, true)?;
            writer.copy_to_user_protected(info_bytes, 0)?;
        }

        // 无论拷贝多少，都返回内核准备好的完整数据大小
        Ok(actual_size as isize)
    }

    pub fn set_ptrace_message(&self, message: usize) {
        self.ptrace_state.lock().set_event_message(message);
    }

    pub fn has_ptrace_option(&self, option: PtraceOptions) -> bool {
        self.ptrace_state.lock().options().contains(option)
    }

    /// 处理 PTRACE_PEEKUSER 请求
    /// - 地址必须按 machine word 对齐
    /// - 目前仅支持读取通用寄存器区域（`struct user_regs_struct` 对应部分）
    /// - 其它 USER 区域（例如 debugreg）暂返回 EIO
    pub fn peek_user(&self, addr: usize) -> Result<isize, SystemError> {
        let word_size = size_of::<usize>();
        if addr & (word_size - 1) != 0 {
            return Err(SystemError::EIO);
        }

        let trap_frame = self.tracee_trap_frame();
        #[cfg(target_arch = "x86_64")]
        let user_regs = {
            let arch_info = self.arch_info_irqsave();
            let fs_base = arch_info.fsbase() as u64;
            let gs_base = arch_info.gsbase() as u64;
            let fs = arch_info.fs() as u64;
            let gs = arch_info.gs() as u64;
            drop(arch_info);
            UserRegsStruct::from_trap_frame(trap_frame, fs_base, gs_base, fs, gs)
        };
        #[cfg(not(target_arch = "x86_64"))]
        let user_regs = UserRegsStruct::from_trap_frame(trap_frame);

        if addr + word_size > size_of::<UserRegsStruct>() {
            return Err(SystemError::EIO);
        }

        let regs_base = &user_regs as *const UserRegsStruct as *const u8;
        let value = unsafe { core::ptr::read_unaligned(regs_base.add(addr) as *const usize) };
        Ok(value as isize)
    }

    /// 设置PTRACE选项
    pub fn set_ptrace_options(&self, options: PtraceOptions) -> Result<(), SystemError> {
        let mut state = self.ptrace_state.lock();
        state.options = options;
        Ok(())
    }
}
