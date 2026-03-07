use crate::{
    arch::{
        interrupt::{TrapFrame, UserRegsStruct},
        ipc::signal::{SigSet, Signal},
        syscall::nr::SYS_PTRACE,
        MMArch,
    },
    filesystem::vfs::iov::IoVec,
    ipc::signal_types::{
        OriginCode, PosixSigInfo, SigChldInfo, SigCode, SigFaultInfo, SigInfo, SigType,
    },
    mm::{MemoryManagementArch, PhysAddr, VirtAddr},
    process::{
        ptrace::{PtraceOptions, PtraceRequest},
        ProcessControlBlock, ProcessManager, ProcessState, RawPid,
    },
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{UserBufferReader, UserBufferWriter},
    },
};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::{cmp::min, slice};
use system_error::SystemError;

const NT_PRSTATUS: usize = 1;
const X86_REGSET_WORD_SIZE: usize = core::mem::size_of::<usize>();

impl TryFrom<usize> for PtraceRequest {
    type Error = SystemError;

    fn try_from(value: usize) -> Result<Self, SystemError> {
        match value {
            0 => Ok(PtraceRequest::Traceme),
            2 => Ok(PtraceRequest::Peekdata),
            3 => Ok(PtraceRequest::Peekuser),
            5 => Ok(PtraceRequest::Pokedata),
            7 => Ok(PtraceRequest::Cont),
            9 => Ok(PtraceRequest::Singlestep),
            12 => Ok(PtraceRequest::Getregs),
            13 => Ok(PtraceRequest::Setregs),
            16 => Ok(PtraceRequest::Attach),
            17 => Ok(PtraceRequest::Detach),
            24 => Ok(PtraceRequest::Syscall),
            31 => Ok(PtraceRequest::Sysemu),
            32 => Ok(PtraceRequest::SysemuSinglestep),
            0x4200 => Ok(PtraceRequest::Setoptions),
            0x4202 => Ok(PtraceRequest::Getsiginfo),
            0x4203 => Ok(PtraceRequest::Setsiginfo),
            0x4204 => Ok(PtraceRequest::Getregset),
            0x4205 => Ok(PtraceRequest::Setregset),
            0x4206 => Ok(PtraceRequest::Seize),
            0x420a => Ok(PtraceRequest::Getsigmask),
            0x420b => Ok(PtraceRequest::Setsigmask),
            0x420e => Ok(PtraceRequest::Getsyscallinfo),
            _ => Err(SystemError::EINVAL),
        }
    }
}

fn ptrace_siginfo_pid(pid: i32) -> RawPid {
    if pid < 0 {
        RawPid(0)
    } else {
        RawPid(pid as usize)
    }
}

fn ptrace_siginfo_type(signal: Signal, si_code: i32, user_info: &PosixSigInfo) -> SigType {
    if si_code == i32::from(SigCode::Origin(OriginCode::Queue)) {
        let rt = unsafe { user_info._sifields._rt };
        return SigType::Rt {
            pid: ptrace_siginfo_pid(rt.si_pid),
            uid: rt.si_uid,
            sigval: rt.si_sigval,
        };
    }

    if si_code == i32::from(SigCode::Origin(OriginCode::Timer)) {
        let timer = unsafe { user_info._sifields._timer };
        return SigType::PosixTimer {
            timerid: timer.si_tid,
            overrun: timer.si_overrun,
            sigval: timer.si_sigval,
        };
    }

    if signal == Signal::SIGCHLD {
        let chld = unsafe { user_info._sifields._sigchld };
        return SigType::SigChld(SigChldInfo {
            pid: ptrace_siginfo_pid(chld.si_pid),
            uid: chld.si_uid as usize,
            status: chld.si_status,
            utime: chld.si_utime as u64,
            stime: chld.si_stime as u64,
        });
    }

    if matches!(
        signal,
        Signal::SIGILL | Signal::SIGFPE | Signal::SIGSEGV | Signal::SIGBUS | Signal::SIGTRAP
    ) {
        let fault = unsafe { user_info._sifields._sigfault };
        return SigType::SigFault(SigFaultInfo {
            addr: fault.si_addr as usize,
            trapno: 0,
        });
    }

    let kill = unsafe { user_info._sifields._kill };
    SigType::Kill {
        pid: ptrace_siginfo_pid(kill.si_pid),
        uid: kill.si_uid,
    }
}

fn ptrace_siginfo_from_user(data: usize) -> Result<SigInfo, SystemError> {
    let reader = UserBufferReader::new(
        data as *const u8,
        core::mem::size_of::<PosixSigInfo>(),
        true,
    )?;
    let buffer = reader.buffer_protected(0)?;
    let user_info = buffer.read_one::<PosixSigInfo>(0)?;

    let signal = Signal::from(user_info.si_signo);
    if signal == Signal::INVALID {
        return Err(SystemError::EINVAL);
    }

    let sig_code =
        SigCode::try_from_i32(user_info.si_code).unwrap_or(SigCode::Raw(user_info.si_code));
    let sig_type = ptrace_siginfo_type(signal, user_info.si_code, &user_info);

    Ok(SigInfo::new(signal, user_info.si_errno, sig_code, sig_type))
}

/// ptrace 内存访问辅助函数
///
/// 按照的 ptrace_access_vm 模式实现，但不使用页表切换：
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
fn ptrace_peek_data(
    tracee: &Arc<ProcessControlBlock>,
    addr: usize,
    _data: usize,
) -> Result<isize, SystemError> {
    let tracee_vm = tracee.basic().user_vm().ok_or(SystemError::ESRCH)?;
    let tracee_vm_guard = tracee_vm.read();

    // 尝试读取 sizeof(unsigned long) 字节（通常是 8 字节）
    const WORD_SIZE: usize = core::mem::size_of::<u64>();

    // 检查地址是否在 tracee 的地址空间中
    let tracee_addr = VirtAddr::new(addr);
    // 注意：contains 可能失败，但这不一定表示错误，因为地址可能在页表中映射但不在 VMA 中
    // 因此我们只在找到 VMA 时使用它进行额外检查，否则继续尝试 translate
    let _vma = tracee_vm_guard.mappings.contains(tracee_addr);

    // 尝试直接读取，使用异常表保护
    let mut value: u64 = 0;

    // 处理可能的跨页边界访问
    let page_offset = addr & (MMArch::PAGE_SIZE - 1);
    let bytes_to_end = MMArch::PAGE_SIZE - page_offset;

    unsafe {
        if bytes_to_end >= WORD_SIZE {
            // 单页访问，一次性读取
            let tracee_phys = match tracee_vm_guard.user_mapper.utable.translate(tracee_addr) {
                Some((phys_frame, _)) => PhysAddr::new(phys_frame.data() + page_offset),
                None => {
                    // translate 失败，可能是页面未映射或权限不足
                    // 这时我们尝试使用异常表保护的直接访问
                    // 如果仍然失败，返回 EIO
                    return Err(SystemError::EIO);
                }
            };

            let kernel_virt = MMArch::phys_2_virt(tracee_phys).ok_or(SystemError::EIO)?;
            let src_ptr = kernel_virt.data() as *const u8;
            let dst_ptr = &mut value as *mut u64 as *mut u8;

            let result = MMArch::copy_with_exception_table(dst_ptr, src_ptr, WORD_SIZE);
            if result != 0 {
                return Err(SystemError::EIO);
            }
        } else {
            // 跨页访问，需要分两次读取
            // 第一页：bytes_to_end 字节
            let tracee_phys1 = match tracee_vm_guard.user_mapper.utable.translate(tracee_addr) {
                Some((phys_frame, _)) => PhysAddr::new(phys_frame.data() + page_offset),
                None => return Err(SystemError::EIO),
            };
            let kernel_virt1 = MMArch::phys_2_virt(tracee_phys1).ok_or(SystemError::EIO)?;
            let src_ptr1 = kernel_virt1.data() as *const u8;
            let dst_ptr = &mut value as *mut u64 as *mut u8;

            let result1 = MMArch::copy_with_exception_table(dst_ptr, src_ptr1, bytes_to_end);
            if result1 != 0 {
                return Err(SystemError::EIO);
            }

            // 第二页：WORD_SIZE - bytes_to_end 字节
            let tracee_addr2 = VirtAddr::new(addr + bytes_to_end);
            if tracee_vm_guard.mappings.contains(tracee_addr2).is_none() {
                return Err(SystemError::EIO);
            }

            let tracee_phys2 = match tracee_vm_guard.user_mapper.utable.translate(tracee_addr2) {
                Some((phys_frame, _)) => PhysAddr::new(phys_frame.data()),
                None => return Err(SystemError::EIO),
            };
            let kernel_virt2 = MMArch::phys_2_virt(tracee_phys2).ok_or(SystemError::EIO)?;
            let src_ptr2 = kernel_virt2.data() as *const u8;
            let dst_ptr2 = (dst_ptr as usize + bytes_to_end) as *mut u8;

            let result2 =
                MMArch::copy_with_exception_table(dst_ptr2, src_ptr2, WORD_SIZE - bytes_to_end);
            if result2 != 0 {
                return Err(SystemError::EIO);
            }
        }
    }

    drop(tracee_vm_guard);

    Ok(value as isize)
}

/// 向 tracee 的用户空间写入数据（安全版本）
///
/// 使用物理地址翻译避免页表切换，不关闭中断。
fn ptrace_poke_data(
    tracee: &Arc<ProcessControlBlock>,
    addr: usize,
    data: usize,
) -> Result<isize, SystemError> {
    let tracee_vm = tracee.basic().user_vm().ok_or(SystemError::ESRCH)?;
    let tracee_vm_guard = tracee_vm.read();

    // 尝试写入 sizeof(unsigned long) 字节（通常是 8 字节）
    const WORD_SIZE: usize = core::mem::size_of::<u64>();

    // 检查地址是否在 tracee 的地址空间中
    let tracee_addr = VirtAddr::new(addr);
    if tracee_vm_guard.mappings.contains(tracee_addr).is_none() {
        return Err(SystemError::EIO);
    }

    let value: u64 = data as u64;

    // 处理可能的跨页边界访问
    let page_offset = addr & (MMArch::PAGE_SIZE - 1);
    let bytes_to_end = MMArch::PAGE_SIZE - page_offset;

    unsafe {
        if bytes_to_end >= WORD_SIZE {
            // 单页访问，一次性写入
            let tracee_phys = match tracee_vm_guard.user_mapper.utable.translate(tracee_addr) {
                Some((phys_frame, _)) => PhysAddr::new(phys_frame.data() + page_offset),
                None => return Err(SystemError::EIO),
            };

            let kernel_virt = MMArch::phys_2_virt(tracee_phys).ok_or(SystemError::EIO)?;
            let src_ptr = &value as *const u64 as *const u8;
            let dst_ptr = kernel_virt.data() as *mut u8;

            let result = MMArch::copy_with_exception_table(dst_ptr, src_ptr, WORD_SIZE);
            if result != 0 {
                return Err(SystemError::EIO);
            }
        } else {
            // 跨页访问，需要分两次写入
            // 第一页：bytes_to_end 字节
            let tracee_phys1 = match tracee_vm_guard.user_mapper.utable.translate(tracee_addr) {
                Some((phys_frame, _)) => PhysAddr::new(phys_frame.data() + page_offset),
                None => return Err(SystemError::EIO),
            };
            let kernel_virt1 = MMArch::phys_2_virt(tracee_phys1).ok_or(SystemError::EIO)?;
            let src_ptr = &value as *const u64 as *const u8;
            let dst_ptr1 = kernel_virt1.data() as *mut u8;

            let result1 = MMArch::copy_with_exception_table(dst_ptr1, src_ptr, bytes_to_end);
            if result1 != 0 {
                return Err(SystemError::EIO);
            }

            // 第二页：WORD_SIZE - bytes_to_end 字节
            let tracee_addr2 = VirtAddr::new(addr + bytes_to_end);
            if tracee_vm_guard.mappings.contains(tracee_addr2).is_none() {
                return Err(SystemError::EIO);
            }

            let tracee_phys2 = match tracee_vm_guard.user_mapper.utable.translate(tracee_addr2) {
                Some((phys_frame, _)) => PhysAddr::new(phys_frame.data()),
                None => return Err(SystemError::EIO),
            };
            let kernel_virt2 = MMArch::phys_2_virt(tracee_phys2).ok_or(SystemError::EIO)?;
            let src_ptr2 = (src_ptr as usize + bytes_to_end) as *const u8;
            let dst_ptr2 = kernel_virt2.data() as *mut u8;

            let result2 =
                MMArch::copy_with_exception_table(dst_ptr2, src_ptr2, WORD_SIZE - bytes_to_end);
            if result2 != 0 {
                return Err(SystemError::EIO);
            }
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
    /// - 不发送 SIGSTOP 给 tracee
    /// - addr 参数必须为 0
    /// - data 参数包含 ptrace 选项
    fn handle_seize(
        tracer: &Arc<ProcessControlBlock>,
        tracee_pid: RawPid,
        addr: usize,
        data: usize,
    ) -> Result<isize, SystemError> {
        if addr != 0 {
            return Err(SystemError::EIO);
        }
        let tracee = ProcessManager::find(tracee_pid).ok_or(SystemError::ESRCH)?;
        // data 参数包含 ptrace 选项
        let options = PtraceOptions::from_bits(data).ok_or(SystemError::EINVAL)?;
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

    /// 处理 PTRACE_SETSIGINFO 请求
    fn handle_set_siginfo(
        tracee: &Arc<ProcessControlBlock>,
        data: usize,
    ) -> Result<isize, SystemError> {
        let siginfo = ptrace_siginfo_from_user(data)?;
        tracee.ptrace_setsiginfo(siginfo)?;
        Ok(0)
    }

    /// 处理 PTRACE_GETSIGMASK 请求
    fn handle_get_sigmask(
        tracee: &Arc<ProcessControlBlock>,
        user_size: usize,
        data: usize,
    ) -> Result<isize, SystemError> {
        if user_size != core::mem::size_of::<SigSet>() {
            return Err(SystemError::EINVAL);
        }
        let sigmask = tracee.ptrace_get_sigmask();
        let mut writer =
            UserBufferWriter::new(data as *mut u8, core::mem::size_of::<SigSet>(), true)?;
        writer.copy_one_to_user(&sigmask, 0)?;
        Ok(0)
    }

    /// 处理 PTRACE_SETSIGMASK 请求（更新 tracee 的 blocked mask）
    fn handle_set_sigmask(
        tracee: &Arc<ProcessControlBlock>,
        user_size: usize,
        data: usize,
    ) -> Result<isize, SystemError> {
        if user_size != core::mem::size_of::<SigSet>() {
            return Err(SystemError::EINVAL);
        }
        let reader =
            UserBufferReader::new(data as *const u8, core::mem::size_of::<SigSet>(), true)?;
        let mut sigmask = SigSet::empty();
        reader.copy_one_from_user(&mut sigmask, 0)?;
        tracee.ptrace_set_sigmask(sigmask);
        Ok(0)
    }

    /// 处理 PTRACE_GET_SYSCALL_INFO 请求
    fn handle_get_syscall_info(
        tracee: &Arc<ProcessControlBlock>,
        user_size: usize,
        data: usize,
    ) -> Result<isize, SystemError> {
        tracee.ptrace_get_syscall_info(user_size, data)
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
        data: usize,
    ) -> Result<isize, SystemError> {
        ptrace_peek_data(tracee, addr, data)
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

    /// 处理 PTRACE_GETREGS 请求 (获取寄存器值)
    fn handle_get_regs(
        tracee: &Arc<ProcessControlBlock>,
        data: usize,
    ) -> Result<isize, SystemError> {
        let user_regs = Self::tracee_user_regs(tracee);

        // 拷贝到用户空间
        let mut writer = UserBufferWriter::new(
            data as *mut u8,
            core::mem::size_of::<UserRegsStruct>(),
            true,
        )?;
        writer.copy_one_to_user(&user_regs, 0)?;

        Ok(0)
    }

    /// 处理 PTRACE_SETREGS 请求 (设置寄存器值)
    fn handle_set_regs(
        tracee: &Arc<ProcessControlBlock>,
        data: usize,
    ) -> Result<isize, SystemError> {
        let mut user_regs = UserRegsStruct::default();
        let reader = UserBufferReader::new(
            data as *const u8,
            core::mem::size_of::<UserRegsStruct>(),
            true,
        )?;
        reader.copy_one_from_user(&mut user_regs, 0)?;

        Self::write_tracee_user_regs(tracee, &user_regs);

        Ok(0)
    }

    fn tracee_user_regs(tracee: &Arc<ProcessControlBlock>) -> UserRegsStruct {
        let trap_frame = tracee.tracee_trap_frame();

        #[cfg(target_arch = "x86_64")]
        {
            // 获取 fs_base、gs_base 和段选择器
            let arch_info = tracee.arch_info_irqsave();
            let fs_base = arch_info.fsbase() as u64;
            let gs_base = arch_info.gsbase() as u64;
            let fs = arch_info.fs() as u64;
            let gs = arch_info.gs() as u64;
            drop(arch_info);

            UserRegsStruct::from_trap_frame(trap_frame, fs_base, gs_base, fs, gs)
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            UserRegsStruct::from_trap_frame(trap_frame)
        }
    }

    fn write_tracee_user_regs(tracee: &Arc<ProcessControlBlock>, user_regs: &UserRegsStruct) {
        let trap_frame = unsafe { &mut *tracee.tracee_trap_frame_ptr() };

        user_regs.write_to_trap_frame(trap_frame);

        #[cfg(target_arch = "x86_64")]
        {
            // x86_64 额外字段不在 TrapFrame 中，需要同步到 arch_info。
            let mut arch_info = tracee.arch_info_irqsave();
            arch_info.set_fsbase(user_regs.fs_base as usize);
            arch_info.set_gsbase(user_regs.gs_base as usize);
            arch_info.set_fs(user_regs.fs as u16);
            arch_info.set_gs(user_regs.gs as u16);
        }
    }

    fn ptrace_regset(
        tracee: &Arc<ProcessControlBlock>,
        request: PtraceRequest,
        note_type: usize,
        iov: &mut IoVec,
    ) -> Result<(), SystemError> {
        if note_type != NT_PRSTATUS || !iov.iov_len.is_multiple_of(X86_REGSET_WORD_SIZE) {
            return Err(SystemError::EINVAL);
        }

        let full_len = core::mem::size_of::<UserRegsStruct>();
        iov.iov_len = min(iov.iov_len, full_len);

        match request {
            PtraceRequest::Getregset => {
                let user_regs = Self::tracee_user_regs(tracee);
                let reg_bytes = unsafe {
                    slice::from_raw_parts(
                        (&user_regs as *const UserRegsStruct).cast::<u8>(),
                        full_len,
                    )
                };
                let mut writer = UserBufferWriter::new(iov.iov_base, iov.iov_len, true)?;
                writer
                    .buffer_protected(0)?
                    .write_to_user(0, &reg_bytes[..iov.iov_len])?;
            }
            PtraceRequest::Setregset => {
                let mut user_regs = Self::tracee_user_regs(tracee);
                let reg_bytes = unsafe {
                    slice::from_raw_parts_mut(
                        (&mut user_regs as *mut UserRegsStruct).cast::<u8>(),
                        full_len,
                    )
                };
                let reader = UserBufferReader::new(iov.iov_base, iov.iov_len, true)?;
                reader
                    .buffer_protected(0)?
                    .read_from_user(0, &mut reg_bytes[..iov.iov_len])?;
                Self::write_tracee_user_regs(tracee, &user_regs);
            }
            _ => return Err(SystemError::EINVAL),
        }

        Ok(())
    }

    fn handle_regset(
        tracee: &Arc<ProcessControlBlock>,
        request: PtraceRequest,
        addr: usize,
        data: usize,
    ) -> Result<isize, SystemError> {
        let uiov_reader =
            UserBufferReader::new(data as *const IoVec, core::mem::size_of::<IoVec>(), true)?;
        let mut iov = uiov_reader.buffer_protected(0)?.read_one::<IoVec>(0)?;

        Self::ptrace_regset(tracee, request, addr, &mut iov)?;

        let mut uiov_writer =
            UserBufferWriter::new(data as *mut IoVec, core::mem::size_of::<IoVec>(), true)?;
        uiov_writer.copy_one_to_user(&iov.iov_len, core::mem::offset_of!(IoVec, iov_len))?;
        Ok(0)
    }

    fn ptrace_check_attach(
        tracee: &Arc<ProcessControlBlock>,
        _request: PtraceRequest,
    ) -> Result<(), SystemError> {
        let current = ProcessManager::current_pcb();

        if !tracee.is_traced_by(&current) {
            return Err(SystemError::ESRCH);
        }
        match tracee.sched_info().inner_lock_read_irqsave().state() {
            ProcessState::TracedStopped(_) => Ok(()),
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

        if !matches!(
            request,
            PtraceRequest::Traceme | PtraceRequest::Attach | PtraceRequest::Seize
        ) {
            Self::ptrace_check_attach(&tracee, request)?;
        }

        let result: isize = match request {
            // 读取进程内存
            PtraceRequest::Peekdata => Self::handle_peek_data(&tracee, addr, data)?,
            // 读取用户寄存器
            PtraceRequest::Peekuser => Self::handle_peek_user(&tracee, addr)?,
            // 写入进程内存
            PtraceRequest::Pokedata => Self::handle_poke_data(&tracee, addr, data)?,
            // 继续执行目标进程
            PtraceRequest::Cont
            | PtraceRequest::Singlestep
            | PtraceRequest::Syscall
            | PtraceRequest::Sysemu
            | PtraceRequest::SysemuSinglestep => {
                // data 是要注入的信号编号，0 表示无信号
                // 仅在这里转换 signal，避免对其他 request（如 GETREGS）中 data 是指针时产生误报
                let signal = if data == 0 {
                    None
                } else {
                    Some(Signal::from(data as i32))
                };
                tracee.ptrace_resume(request, signal, frame)?
            }
            // 获取寄存器值
            PtraceRequest::Getregs => Self::handle_get_regs(&tracee, data)?,
            // 设置寄存器值
            PtraceRequest::Setregs => Self::handle_set_regs(&tracee, data)?,
            // 获取寄存器集合
            PtraceRequest::Getregset => Self::handle_regset(&tracee, request, addr, data)?,
            // 设置寄存器集合
            PtraceRequest::Setregset => Self::handle_regset(&tracee, request, addr, data)?,
            // 附加到目标进程
            PtraceRequest::Attach => Self::handle_attach(&tracer, pid)?,
            // 分离目标进程
            PtraceRequest::Detach => {
                // data 是分离时要发送的信号编号，0 表示无信号
                let signal = if data == 0 {
                    None
                } else {
                    Some(Signal::from(data as i32))
                };
                Self::handle_detach(&tracee, signal)?
            }
            // 设置跟踪选项
            PtraceRequest::Setoptions => Self::handle_set_options(&tracee, data)?,
            // 获取信号信息
            PtraceRequest::Getsiginfo => Self::handle_get_siginfo(&tracee, data)?,
            // 设置信号信息
            PtraceRequest::Setsiginfo => Self::handle_set_siginfo(&tracee, data)?,
            // 获取 signal mask
            PtraceRequest::Getsigmask => Self::handle_get_sigmask(&tracee, addr, data)?,
            // 设置 signal mask
            PtraceRequest::Setsigmask => Self::handle_set_sigmask(&tracee, addr, data)?,
            // 获取系统调用停止信息
            PtraceRequest::Getsyscallinfo => Self::handle_get_syscall_info(&tracee, addr, data)?,
            // PTRACE_SEIZE：现代 API，不发送 SIGSTOP
            PtraceRequest::Seize => Self::handle_seize(&tracer, pid, addr, data)?,
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
