use crate::arch::interrupt::TrapFrame;
use crate::arch::ipc::signal::Signal;
use crate::arch::syscall::nr::{SYS_EXIT, SYS_PTRACE};
use crate::process::syscall::sys_exit::SysExit;
use crate::process::{
    ProcessControlBlock, ProcessFlags, ProcessManager, ProcessState, PtraceOptions, PtraceRequest,
    RawPid,
};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

impl TryFrom<usize> for PtraceRequest {
    type Error = SystemError;

    fn try_from(value: usize) -> Result<Self, SystemError> {
        match value {
            0 => Ok(PtraceRequest::PtraceTraceme),
            16 => Ok(PtraceRequest::PtraceAttach),
            17 => Ok(PtraceRequest::PtraceDetach),
            24 => Ok(PtraceRequest::PtraceSyscall),
            9 => Ok(PtraceRequest::PtraceSinglestep),
            7 => Ok(PtraceRequest::PtraceCont),
            12 => Ok(PtraceRequest::PtraceGetregs),
            13 => Ok(PtraceRequest::PtraceSetregs),
            2 => Ok(PtraceRequest::PtracePeekdata),
            5 => Ok(PtraceRequest::PtracePokedata),
            0x4202 => Ok(PtraceRequest::PtraceGetsiginfo),
            0x4200 => Ok(PtraceRequest::PtraceSetoptions),
            _ => Err(SystemError::EINVAL),
        }
    }
}

/// ptrace 系统调用实现
pub struct SysPtrace;

impl SysPtrace {
    fn request(args: &[usize]) -> Result<PtraceRequest, SystemError> {
        PtraceRequest::try_from(args[0]).map_err(|_| SystemError::EINVAL)
    }

    fn pid(args: &[usize]) -> RawPid {
        RawPid(args[1])
    }

    fn addr(args: &[usize]) -> usize {
        args[2]
    }

    fn data(args: &[usize]) -> usize {
        args[3]
    }

    /// 处理 PTRACE_TRACEME 请求（当前进程请求被跟踪）
    fn handle_traceme(tracer: &Arc<ProcessControlBlock>) -> Result<isize, SystemError> {
        tracer.traceme()
    }

    /// 处理 PTRACE_ATTACH 请求（附加到目标进程）
    fn handle_attach(
        tracer: &Arc<ProcessControlBlock>,
        tracee_pid: RawPid,
    ) -> Result<isize, SystemError> {
        let tracee = ProcessManager::find(tracee_pid).ok_or(SystemError::ESRCH)?;
        tracee.attach(tracer)
    }

    /// 处理 PTRACE_DETACH 请求（分离目标进程）
    fn handle_detach(
        tracee: &Arc<ProcessControlBlock>,
        signal: Option<Signal>,
    ) -> Result<isize, SystemError> {
        // 验证调用者是跟踪器
        if ProcessManager::current_pcb().raw_pid() != tracee.tracer().unwrap() {
            return Err(SystemError::EPERM);
        }
        tracee.detach(signal)
    }

    /// 处理 PTRACE_CONT 请求
    fn handle_cont(
        tracee: &ProcessControlBlock,
        signal: Option<Signal>,
    ) -> Result<isize, SystemError> {
        tracee.ptrace_cont(signal)
    }

    /// 处理 PTRACE_SYSCALL 请求（在系统调用入口和出口暂停）
    fn handle_syscall(tracee: &Arc<ProcessControlBlock>) -> Result<isize, SystemError> {
        // 检查调用者是否是该进程的跟踪器
        if ProcessManager::current_pcb().raw_pid() != tracee.tracer().unwrap() {
            // TODO
            return Err(SystemError::ESRCH);
        }
        // 设置系统调用跟踪标志
        tracee.enable_syscall_tracing();
        tracee.trace_syscall()
    }

    /// 处理 PTRACE_SETOPTIONS 请求（设置跟踪选项）
    fn handle_set_options(
        tracee: &Arc<ProcessControlBlock>,
        data: usize,
    ) -> Result<isize, SystemError> {
        let options = PtraceOptions::from_bits_truncate(data);
        // 设置跟踪选项
        tracee.set_ptrace_options(options)?;

        Ok(0)
    }

    /// 处理 PTRACE_GETSIGINFO 请求（获取信号信息）
    fn handle_get_siginfo(_tracee: &Arc<ProcessControlBlock>) -> Result<isize, SystemError> {
        // 在实际实现中，你需要获取并返回信号信息
        // 这里仅返回占位值
        Ok(0)
    }

    /// 处理 PTRACE_PEEKUSER 请求
    fn handle_peek_user(
        tracee: &Arc<ProcessControlBlock>,
        addr: usize,
    ) -> Result<isize, SystemError> {
        let value = tracee.peek_user(addr)?;
        Ok(value as isize)
    }

    /// 处理 PTRACE_PEEKDATA 请求（读取进程内存）
    fn handle_peek_data(
        tracee: &Arc<ProcessControlBlock>,
        addr: usize,
    ) -> Result<isize, SystemError> {
        // // 检查地址是否在用户空间范围
        // if !tracee.memory.is_valid_user(addr) {
        //     return Err(SystemError::EFAULT);
        // }
        // // 安全读取内存
        // let value = tracee.memory.read(addr)?;
        // Ok(value as isize)
        todo!()
    }

    /// 处理 PTRACE_SINGLESTEP 请求 (单步执行)
    fn handle_single_step(tracee: &Arc<ProcessControlBlock>) -> Result<isize, SystemError> {
        // 检查调用者是否是该进程的跟踪器
        if ProcessManager::current_pcb().raw_pid() != tracee.tracer().unwrap() {
            // TODO
            return Err(SystemError::ESRCH);
        }
        // 设置 EFLAGS 的 TF 标志
        tracee.enable_single_step();
        // 恢复进程运行
        let mut sched_info = tracee.sched_info.inner_lock_write_irqsave();
        if let ProcessState::Stopped(_signal) = sched_info.state() {
            sched_info.set_state(ProcessState::Runnable);
        }
        Ok(0)
    }

    /// 处理 PTRACE_GETREGS 请求 (获取寄存器值)
    fn handle_get_regs(tracee: &Arc<ProcessControlBlock>) -> Result<isize, SystemError> {
        // let tf = tracee.context().trap_frame.as_ref();
        Ok(0) // 实际应返回寄存器结构体
    }

    /// 处理 PTRACE_SETREGS 请求 (设置寄存器值)
    fn handle_set_regs(
        _tracee: &Arc<ProcessControlBlock>,
        _data: usize,
    ) -> Result<isize, SystemError> {
        // 从用户空间复制寄存器结构体
        Ok(0)
    }

    // 在系统调用处理之前
    fn before_handle_syscall(num: usize, args: &[usize]) {
        let current = ProcessManager::current_pcb();
        // 检查进程是否被跟踪并且启用了系统调用跟踪
        if current
            .flags()
            .contains(ProcessFlags::PTRACED | ProcessFlags::TRACE_SYSCALL)
        {
            // 保存系统调用信息
            current.on_syscall_entry(num, args);
            // 暂停进程等待跟踪器
            current.set_state(ProcessState::Stopped(1));
            // Scheduler::schedule(SchedMode::SM_NONE); // 切换到其他进程
        }
    }

    // 在系统调用处理之后
    fn after_handle_syscall(num: usize, result: isize) {
        let current = ProcessManager::current_pcb();
        // 检查进程是否被跟踪并且启用了系统调用跟踪
        if current
            .flags()
            .contains(ProcessFlags::PTRACED | ProcessFlags::TRACE_SYSCALL)
        {
            // 保存系统调用结果
            current.on_syscall_exit(result);
            // 暂停进程等待跟踪器
            current.set_state(ProcessState::Stopped(1));
            // Scheduler::schedule(SchedMode::SM_NONE); // 切换到其他进程
        }
    }

    // 在系统调用分发函数中
    fn dispatch_syscall(
        num: usize,
        args: &[usize],
        frame: &mut TrapFrame,
    ) -> Result<usize, SystemError> {
        Self::before_handle_syscall(num, args);

        // 执行实际的系统调用处理
        let result = match num {
            SYS_EXIT => SysExit.handle(args, frame)?,
            // ... 其他系统调用 ...
            _ => Err(SystemError::ENOSYS)?,
        };

        Self::after_handle_syscall(num, result as isize);
        Ok(result)
    }
}

impl Syscall for SysPtrace {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        if args.len() < 4 {
            return Err(SystemError::EINVAL);
        }

        let request = Self::request(args)?;
        let pid = Self::pid(args);
        let addr = Self::addr(args);
        let data = Self::data(args);

        let tracer = ProcessManager::current_pcb();
        if request == PtraceRequest::PtraceTraceme {
            return Self::handle_traceme(&tracer).map(|r| r as usize);
        }
        let tracee: Arc<ProcessControlBlock> =
            ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
        let signal: Option<Signal> = if data == 0 {
            None // 表示无信号
        } else {
            Some(Signal::try_from(data as i32).map_err(|_| SystemError::EINVAL)?)
        };

        let result: isize = match request {
            // 附加到目标进程
            PtraceRequest::PtraceAttach => Self::handle_attach(&tracer, pid)?,
            // 分离目标进程
            PtraceRequest::PtraceDetach => Self::handle_detach(&tracee, signal)?,
            // 继续执行目标进程
            PtraceRequest::PtraceCont => Self::handle_cont(&tracee, signal)?,
            // 在系统调用入口和出口暂停
            PtraceRequest::PtraceSyscall => Self::handle_syscall(&tracee)?,
            // 设置跟踪选项
            PtraceRequest::PtraceSetoptions => Self::handle_set_options(&tracee, data)?,
            // 获取信号信息
            PtraceRequest::PtraceGetsiginfo => Self::handle_get_siginfo(&tracee)?,
            // 读取用户寄存器
            PtraceRequest::PtracePeekuser => Self::handle_peek_user(&tracee, addr)?,
            // 读取进程内存
            PtraceRequest::PtracePeekdata => Self::handle_peek_data(&tracee, addr)?,
            PtraceRequest::PtraceSinglestep => todo!(),
            // 其他请求类型
            _ => {
                log::warn!("Unimplemented ptrace request: {:?}", request);
                0
            }
        };

        Ok(result as usize)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        let request_name = match PtraceRequest::try_from(args[0]) {
            Ok(req) => format!("{:?}", req),
            Err(_) => format!("{:#x}", args[0]),
        };

        vec![
            FormattedSyscallParam::new("request", request_name),
            FormattedSyscallParam::new("pid", format!("{}", args[1])),
            FormattedSyscallParam::new("addr", format!("{:#x}", args[2])),
            FormattedSyscallParam::new("data", format!("{:#x}", args[3])),
        ]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_PTRACE, SysPtrace);
