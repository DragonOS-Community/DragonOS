use crate::{
    arch::{
        interrupt::TrapFrame,
        ipc::signal::Signal,
        syscall::nr::{SYS_EXIT, SYS_PTRACE},
        MMArch,
    },
    ipc::signal_types::PosixSigInfo,
    mm::{MemoryManagementArch, PhysAddr, VirtAddr},
    process::{
        syscall::sys_exit::SysExit, ProcessControlBlock, ProcessFlags, ProcessManager,
        ProcessState, PtraceOptions, PtraceRequest, RawPid,
    },
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferWriter,
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
    #[allow(dead_code)]
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

/// ptrace 内存访问辅助函数
///
/// 按照 Linux 6.6 的 ptrace_access_vm 模式实现，但不使用页表切换：
/// - 直接将 tracee 的虚拟地址翻译为物理地址
/// - 通过 phys_2_virt 映射到内核虚拟地址空间
/// - 使用异常表保护的拷贝函数，安全处理缺页异常
/// - **不关闭中断**，避免中断禁用期间缺页导致的死锁
///
/// # Safety
/// 调用者必须确保 tracee 在访问期间不会被销毁
#[allow(dead_code)]
fn ptrace_access_vm<F, R>(tracee: &Arc<ProcessControlBlock>, f: F) -> Result<R, SystemError>
where
    F: FnOnce() -> Result<R, SystemError>,
{
    // 获取目标进程的地址空间
    let tracee_vm = tracee.basic().user_vm().ok_or(SystemError::ESRCH)?;

    // 获取目标进程的地址空间锁，但不切换页表
    // 只需要在地址空间读锁保护下执行操作
    let _tracee_vm_guard = tracee_vm.read();

    // 在目标进程的地址空间读锁保护中执行操作
    f()
}

/// 从 tracee 的用户空间读取数据（安全版本）
///
/// 使用物理地址翻译避免页表切换，不关闭中断。
/// 参考 process_vm_readv 的实现方式。
fn ptrace_peek_data(tracee: &Arc<ProcessControlBlock>, addr: usize) -> Result<isize, SystemError> {
    let tracee_vm = tracee.basic().user_vm().ok_or(SystemError::ESRCH)?;
    let tracee_vm_guard = tracee_vm.read();

    let tracee_addr = VirtAddr::new(addr);

    // 检查地址是否在 tracee 的地址空间中
    if tracee_vm_guard.mappings.contains(tracee_addr).is_none() {
        return Err(SystemError::EIO);
    }

    // 计算页内偏移
    let page_offset = addr & (MMArch::PAGE_SIZE - 1);

    // 翻译 tracee 的虚拟地址为物理地址
    let tracee_phys = match tracee_vm_guard.user_mapper.utable.translate(tracee_addr) {
        Some((phys_frame, _)) => PhysAddr::new(phys_frame.data() + page_offset),
        None => return Err(SystemError::EIO),
    };
    drop(tracee_vm_guard);

    // 使用异常表保护的拷贝
    let mut value: u64 = 0;
    unsafe {
        // 将物理地址映射为内核虚拟地址
        let kernel_virt = MMArch::phys_2_virt(tracee_phys).ok_or(SystemError::EIO)?;

        let src_ptr = kernel_virt.data() as *const u8;
        let dst_ptr = &mut value as *mut u64 as *mut u8;
        let result = MMArch::copy_with_exception_table(dst_ptr, src_ptr, 8);
        if result != 0 {
            return Err(SystemError::EIO);
        }
    }

    Ok(value as isize)
}

/// 向 tracee 的用户空间写入数据（安全版本）
///
/// 使用物理地址翻译避免页表切换，不关闭中断。
/// 参考 process_vm_writev 的实现方式。
fn ptrace_poke_data(
    tracee: &Arc<ProcessControlBlock>,
    addr: usize,
    data: usize,
) -> Result<isize, SystemError> {
    let tracee_vm = tracee.basic().user_vm().ok_or(SystemError::ESRCH)?;
    let tracee_vm_guard = tracee_vm.read();

    let tracee_addr = VirtAddr::new(addr);

    // 检查地址是否在 tracee 的地址空间中
    if tracee_vm_guard.mappings.contains(tracee_addr).is_none() {
        return Err(SystemError::EIO);
    }

    // 计算页内偏移
    let page_offset = addr & (MMArch::PAGE_SIZE - 1);

    // 翻译 tracee 的虚拟地址为物理地址
    let tracee_phys = match tracee_vm_guard.user_mapper.utable.translate(tracee_addr) {
        Some((phys_frame, _)) => PhysAddr::new(phys_frame.data() + page_offset),
        None => return Err(SystemError::EIO),
    };
    drop(tracee_vm_guard);

    // 使用异常表保护的拷贝
    let value: u64 = data as u64;
    unsafe {
        // 将物理地址映射为内核虚拟地址
        let kernel_virt = MMArch::phys_2_virt(tracee_phys).ok_or(SystemError::EIO)?;

        let src_ptr = &value as *const u64 as *const u8;
        let dst_ptr = kernel_virt.data() as *mut u8;
        let result = MMArch::copy_with_exception_table(dst_ptr, src_ptr, 8);
        if result != 0 {
            return Err(SystemError::EIO);
        }
    }

    Ok(0)
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    fn handle_get_siginfo(
        tracee: &Arc<ProcessControlBlock>,
        data: usize,
    ) -> Result<isize, SystemError> {
        // 读取 last_siginfo 并拷贝到用户空间
        let siginfo = tracee
            .ptrace_state
            .lock()
            .last_siginfo()
            .ok_or(SystemError::EINVAL)?;

        // 将 siginfo 转换为 PosixSigInfo 格式并拷贝到用户空间
        let uinfo = data as *mut PosixSigInfo;
        siginfo.copy_posix_siginfo_to_user(uinfo)?;
        log::debug!("PTRACE_GETSIGINFO: siginfo={:?}", siginfo);
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
    /// 使用安全的物理地址翻译方式访问目标进程地址空间：
    /// - 不进行页表切换
    /// - 不关闭中断
    /// - 使用异常表保护安全处理缺页
    fn handle_peek_data(
        tracee: &Arc<ProcessControlBlock>,
        addr: usize,
    ) -> Result<isize, SystemError> {
        ptrace_peek_data(tracee, addr)
    }

    /// 处理 PTRACE_POKEDATA 请求（写入进程内存）
    ///
    /// 使用安全的物理地址翻译方式访问目标进程地址空间：
    /// - 不进行页表切换
    /// - 不关闭中断
    /// - 使用异常表保护安全处理缺页
    fn handle_poke_data(
        tracee: &Arc<ProcessControlBlock>,
        addr: usize,
        data: usize,
    ) -> Result<isize, SystemError> {
        ptrace_poke_data(tracee, addr, data)
    }

    /// 处理 PTRACE_SINGLESTEP 请求 (单步执行)
    #[allow(dead_code)]
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

        // 获取 fs_base、gs_base 和段选择器
        // - fs_base/gs_base: task->thread.fsbase/gsbase
        // - fs/gs: task->thread.fsindex/gsindex
        // - ds/es: task->thread.ds/es 或 pt_regs->ds/es
        let arch_info = tracee.arch_info_irqsave();
        let fs_base = arch_info.fsbase() as u64;
        let gs_base = arch_info.gsbase() as u64;
        let fs = arch_info.fs() as u64;
        let gs = arch_info.gs() as u64;
        drop(arch_info);

        // TrapFrame 包含 ds 和 es（对应 pt_regs->ds/es）
        let ds = trap_frame.ds as u64;
        let es = trap_frame.es as u64;

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
            ds,
            es,
            fs,
            gs,
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    fn after_handle_syscall(_num: usize, result: isize) {
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
    #[allow(dead_code)]
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

    #[allow(dead_code)]
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
            PtraceRequest::Getsiginfo => Self::handle_get_siginfo(&tracee, data)?,
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
