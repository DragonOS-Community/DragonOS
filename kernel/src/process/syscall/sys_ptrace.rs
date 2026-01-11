use crate::{
    arch::{
        interrupt::TrapFrame,
        ipc::signal::Signal,
        syscall::nr::{SYS_EXIT, SYS_PTRACE},
        CurrentIrqArch, MMArch,
    },
    exception::{InterruptArch, IrqFlagsGuard},
    mm::{MemoryManagementArch, PageTableKind, PhysAddr, VirtAddr},
    process::{
        syscall::sys_exit::SysExit, ProcessControlBlock, ProcessFlags, ProcessManager,
        ProcessState, PtraceOptions, PtraceRequest, RawPid,
    },
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{copy_from_user_protected, copy_to_user_protected, UserBufferWriter},
    },
};
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

/// Linux 兼容的用户寄存器结构体 (x86_64)
/// 参考 /usr/include/x86_64-linux-gnu/sys/user.h
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct user_regs_struct {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub orig_rax: u64,
    pub rip: u64,
    pub cs: u64,
    pub eflags: u64,
    pub rsp: u64,
    pub ss: u64,
    pub fs_base: u64,
    pub gs_base: u64,
    pub ds: u64,
    pub es: u64,
    pub fs: u64,
    pub gs: u64,
}

/// 从 TrapFrame 和额外的寄存器信息构建 user_regs_struct
impl user_regs_struct {
    /// 从 TrapFrame 转换，需要提供 fs_base 和 gs_base
    ///
    /// # Safety
    ///
    /// 调用者必须确保 trap_frame 指向的内存有效
    pub unsafe fn from_trap_frame_extra(
        trap_frame: &TrapFrame,
        fs_base: u64,
        gs_base: u64,
    ) -> Self {
        Self {
            r15: trap_frame.r15,
            r14: trap_frame.r14,
            r13: trap_frame.r13,
            r12: trap_frame.r12,
            rbp: trap_frame.rbp,
            rbx: trap_frame.rbx,
            r11: trap_frame.r11,
            r10: trap_frame.r10,
            r9: trap_frame.r9,
            r8: trap_frame.r8,
            rax: trap_frame.rax,
            rcx: trap_frame.rcx,
            rdx: trap_frame.rdx,
            rsi: trap_frame.rsi,
            rdi: trap_frame.rdi,
            orig_rax: trap_frame.errcode, // syscall number
            rip: trap_frame.rip,
            cs: trap_frame.cs,
            eflags: trap_frame.rflags,
            rsp: trap_frame.rsp,
            ss: trap_frame.ss,
            fs_base,
            gs_base,
            ds: 0,
            es: 0,
            fs: 0,
            gs: 0,
        }
    }
}

impl TryFrom<usize> for PtraceRequest {
    type Error = SystemError;

    fn try_from(value: usize) -> Result<Self, SystemError> {
        match value {
            0 => Ok(PtraceRequest::Traceme),
            2 => Ok(PtraceRequest::Peekdata),
            5 => Ok(PtraceRequest::Pokedata),
            7 => Ok(PtraceRequest::Cont),
            9 => Ok(PtraceRequest::Singlestep),
            12 => Ok(PtraceRequest::Getregs),
            13 => Ok(PtraceRequest::Setregs),
            16 => Ok(PtraceRequest::Attach),
            17 => Ok(PtraceRequest::Detach),
            24 => Ok(PtraceRequest::Syscall),
            0x4200 => Ok(PtraceRequest::Setoptions),
            0x4202 => Ok(PtraceRequest::Getsiginfo),
            0x4206 => Ok(PtraceRequest::Seize),
            _ => Err(SystemError::EINVAL),
        }
    }
}

/// 页表切换守卫，用于在作用域结束时自动恢复页表
///
/// 按照 Linux 6.6 ptrace_access_vm 的模式：
/// 1. 禁用中断，防止在切换期间发生中断处理
/// 2. 切换到目标进程的页表
/// 3. 在作用域结束时恢复原始页表并重新启用中断
struct PageTableGuard {
    original_paddr: PhysAddr,
    kind: PageTableKind,
    _irq_guard: IrqFlagsGuard,
}

impl PageTableGuard {
    /// 切换到目标进程的页表
    ///
    /// # Safety
    /// 调用者必须确保：
    /// 1. target_paddr 是有效的页表物理地址
    /// 2. 在此守卫存在期间不会发生调度
    fn new(target_paddr: PhysAddr, kind: PageTableKind) -> Self {
        // 1. 首先禁用中断（关键！）
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        // 2. 获取当前页表物理地址用于恢复
        let current_pcb = ProcessManager::current_pcb();
        let original_paddr = current_pcb
            .basic()
            .user_vm()
            .map(|vm| {
                let inner = vm.read_irqsave();
                inner.user_mapper.utable.table().phys()
            })
            .expect("current process must have user VM");

        // 3. 切换到目标进程的页表
        unsafe {
            <MMArch as MemoryManagementArch>::set_table(kind, target_paddr);
        }

        Self {
            original_paddr,
            kind,
            _irq_guard: irq_guard,
        }
    }
}

impl Drop for PageTableGuard {
    fn drop(&mut self) {
        // 恢复原始页表
        unsafe {
            <MMArch as MemoryManagementArch>::set_table(self.kind, self.original_paddr);
        }
        // 中断会在 _irq_guard drop 时自动恢复
    }
}

/// ptrace 内存访问辅助函数
///
/// 按照 Linux 6.6 的 ptrace_access_vm 模式实现：
/// - 使用临时页表切换访问目标进程的地址空间
/// - 在中断禁用状态下进行页表切换
/// - 使用守卫模式确保页表一定会被恢复
///
/// # Safety
/// 调用者必须确保 tracee 在访问期间不会被销毁
fn ptrace_access_vm<F, R>(tracee: &Arc<ProcessControlBlock>, f: F) -> Result<R, SystemError>
where
    F: FnOnce() -> Result<R, SystemError>,
{
    // 获取目标进程的地址空间
    let tracee_vm = tracee.basic().user_vm().ok_or(SystemError::ESRCH)?;

    // 获取目标进程的用户页表物理地址
    let tracee_mapper_paddr = {
        let inner = tracee_vm.read_irqsave();
        inner.user_mapper.utable.table().phys()
    };

    // 使用守卫切换页表，确保一定会恢复
    let _guard = PageTableGuard::new(tracee_mapper_paddr, PageTableKind::User);

    // 在目标进程的地址空间中执行操作
    f()
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

    /// 处理 PTRACE_SEIZE 请求（现代附加 API）
    ///
    /// 按照 Linux 6.6.21 实现：
    /// - 不发送 SIGSTOP 给 tracee
    /// - addr 参数包含 ptrace 选项
    /// - data 参数通常为 0
    fn handle_seize(
        tracer: &Arc<ProcessControlBlock>,
        tracee_pid: RawPid,
        addr: usize,
    ) -> Result<isize, SystemError> {
        let tracee = ProcessManager::find(tracee_pid).ok_or(SystemError::ESRCH)?;
        // addr 参数包含 ptrace 选项
        let options = PtraceOptions::from_bits_truncate(addr);
        tracee.seize(tracer, options)
    }

    /// 处理 PTRACE_DETACH 请求（分离目标进程）
    fn handle_detach(
        tracee: &Arc<ProcessControlBlock>,
        signal: Option<Signal>,
    ) -> Result<isize, SystemError> {
        // 验证调用者是跟踪器
        let tracer_pid = ProcessManager::current_pcb().raw_pid();
        let tracee_tracer = tracee.tracer().ok_or(SystemError::ESRCH)?;
        if tracer_pid != tracee_tracer {
            return Err(SystemError::EPERM);
        }
        tracee.detach(signal)
    }

    /// 处理 PTRACE_SYSCALL 请求（在系统调用入口和出口暂停）
    fn handle_syscall(tracee: &Arc<ProcessControlBlock>) -> Result<isize, SystemError> {
        // 检查调用者是否是该进程的跟踪器
        let tracer_pid = ProcessManager::current_pcb().raw_pid();
        let tracee_tracer = tracee.tracer().ok_or(SystemError::ESRCH)?;
        if tracer_pid != tracee_tracer {
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
        Ok(value)
    }

    /// 处理 PTRACE_PEEKDATA 请求（读取进程内存）
    ///
    /// 按照 Linux 6.6.21 的 ptrace 语义：
    /// - 使用 ptrace_access_vm 模式访问目标进程地址空间
    /// - 在中断禁用状态下进行页表切换
    /// - 使用守卫模式确保页表恢复
    fn handle_peek_data(
        tracee: &Arc<ProcessControlBlock>,
        addr: usize,
    ) -> Result<isize, SystemError> {
        // 使用安全的 ptrace_access_vm 辅助函数
        ptrace_access_vm(tracee, || {
            let mut value: u64 = 0;
            unsafe {
                copy_from_user_protected(
                    core::slice::from_raw_parts_mut(&mut value as *mut u64 as *mut u8, 8),
                    VirtAddr::new(addr),
                )
            }?;
            Ok(value as isize)
        })
        .map_err(|_| SystemError::EIO)
    }

    /// 处理 PTRACE_POKEDATA 请求（写入进程内存）
    ///
    /// 按照 Linux 6.6.21 的 ptrace 语义：
    /// - 使用 ptrace_access_vm 模式访问目标进程地址空间
    /// - 在中断禁用状态下进行页表切换
    /// - 使用守卫模式确保页表恢复
    fn handle_poke_data(
        tracee: &Arc<ProcessControlBlock>,
        addr: usize,
        data: usize,
    ) -> Result<isize, SystemError> {
        // 使用安全的 ptrace_access_vm 辅助函数
        ptrace_access_vm(tracee, || {
            let value: u64 = data as u64;
            unsafe {
                copy_to_user_protected(
                    VirtAddr::new(addr),
                    core::slice::from_raw_parts(&value as *const u64 as *const u8, 8),
                )
            }?;
            Ok(0)
        })
        .map_err(|_| SystemError::EIO)
    }

    /// 处理 PTRACE_SINGLESTEP 请求 (单步执行)
    fn handle_single_step(tracee: &Arc<ProcessControlBlock>) -> Result<isize, SystemError> {
        // 检查调用者是否是该进程的跟踪器
        let tracer_pid = ProcessManager::current_pcb().raw_pid();
        let tracee_tracer = tracee.tracer().ok_or(SystemError::ESRCH)?;
        if tracer_pid != tracee_tracer {
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
    fn handle_get_regs(
        tracee: &Arc<ProcessControlBlock>,
        data: usize,
    ) -> Result<isize, SystemError> {
        // 获取 tracee 的 TrapFrame
        // TrapFrame 位于内核栈顶部：kernel_stack.max_address - size_of::<TrapFrame>()
        let kstack = tracee.kernel_stack();
        let trap_frame_vaddr =
            VirtAddr::new(kstack.stack_max_address().data() - core::mem::size_of::<TrapFrame>());

        // 从 tracee 的内核栈读取 TrapFrame
        let trap_frame = unsafe { &*(trap_frame_vaddr.data() as *const TrapFrame) };

        // 获取 fs_base 和 gs_base
        let arch_info = tracee.arch_info_irqsave();
        let fs_base = arch_info.fsbase() as u64;
        let gs_base = arch_info.gsbase() as u64;
        drop(arch_info);

        // 构造用户态寄存器结构体
        let user_regs = user_regs_struct {
            r15: trap_frame.r15,
            r14: trap_frame.r14,
            r13: trap_frame.r13,
            r12: trap_frame.r12,
            rbp: trap_frame.rbp,
            rbx: trap_frame.rbx,
            r11: trap_frame.r11,
            r10: trap_frame.r10,
            r9: trap_frame.r9,
            r8: trap_frame.r8,
            rax: trap_frame.rax,
            rcx: trap_frame.rcx,
            rdx: trap_frame.rdx,
            rsi: trap_frame.rsi,
            rdi: trap_frame.rdi,
            orig_rax: trap_frame.errcode, // syscall number
            rip: trap_frame.rip,
            cs: trap_frame.cs,
            eflags: trap_frame.rflags,
            rsp: trap_frame.rsp,
            ss: trap_frame.ss,
            fs_base,
            gs_base,
            ds: 0,
            es: 0,
            fs: 0,
            gs: 0,
        };

        // 拷贝到用户空间
        let mut writer = UserBufferWriter::new(
            data as *mut u8,
            core::mem::size_of::<user_regs_struct>(),
            true,
        )?;
        writer.copy_one_to_user(&user_regs, 0)?;

        Ok(0)
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

    fn ptrace_check_attach(
        tracee: &Arc<ProcessControlBlock>,
        _request: PtraceRequest,
    ) -> Result<(), SystemError> {
        let current = ProcessManager::current_pcb();

        if !tracee.is_traced_by(&current) {
            return Err(SystemError::EPERM);
        }
        match tracee.sched_info().inner_lock_read_irqsave().state() {
            ProcessState::Stopped(_) | ProcessState::TracedStopped(_) => Ok(()),
            _ => Err(SystemError::ESRCH),
        }
    }
}

impl Syscall for SysPtrace {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        if args.len() < 4 {
            return Err(SystemError::EINVAL);
        }

        let request = Self::request(args)?;
        let pid = Self::pid(args);
        let addr = Self::addr(args);
        let data = Self::data(args);

        let tracer = ProcessManager::current_pcb();
        if request == PtraceRequest::Traceme {
            return Self::handle_traceme(&tracer).map(|r| r as usize);
        }
        let tracee: Arc<ProcessControlBlock> =
            ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
        let signal: Option<Signal> = if data == 0 {
            None // 表示无信号
        } else {
            Some(Signal::from(data as i32))
        };

        let result: isize = match request {
            // 读取进程内存
            PtraceRequest::Peekdata => Self::handle_peek_data(&tracee, addr)?,
            // 读取用户寄存器
            PtraceRequest::Peekuser => Self::handle_peek_user(&tracee, addr)?,
            // 写入进程内存
            PtraceRequest::Pokedata => Self::handle_poke_data(&tracee, addr, data)?,
            // 继续执行目标进程
            PtraceRequest::Cont | PtraceRequest::Singlestep | PtraceRequest::Syscall => {
                tracee.ptrace_resume(request, signal, frame)?
            }
            // 获取寄存器值
            PtraceRequest::Getregs => Self::handle_get_regs(&tracee, data)?,
            // 设置寄存器值
            PtraceRequest::Setregs => Self::handle_set_regs(&tracee, data)?,
            // 附加到目标进程
            PtraceRequest::Attach => Self::handle_attach(&tracer, pid)?,
            // 分离目标进程
            PtraceRequest::Detach => Self::handle_detach(&tracee, signal)?,
            // 设置跟踪选项
            PtraceRequest::Setoptions => Self::handle_set_options(&tracee, data)?,
            // 获取信号信息
            PtraceRequest::Getsiginfo => Self::handle_get_siginfo(&tracee)?,
            // PTRACE_SEIZE：现代 API，不发送 SIGSTOP
            PtraceRequest::Seize => Self::handle_seize(&tracer, pid, addr)?,
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
