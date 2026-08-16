//! ptrace 系统调用分发。
//!
//! 本文件只做请求解析、参数校验、目标进程查找和 `ptrace_check_attach` 前置检查，
//! 真正的逻辑在 `process::ptrace` 的 PCB 方法中。

use crate::{
    arch::{
        interrupt::TrapFrame,
        ipc::signal::{SigSet, Signal, MAX_SIG_NUM},
        syscall::nr::SYS_PTRACE,
    },
    ipc::signal_types::{PosixSigInfo, SigCode, SigInfo, SigType},
    mm::VirtAddr,
    process::{
        pid::PidType,
        ptrace::{self, PtraceRequest},
        ProcessManager, RawPid,
    },
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysPtrace;

impl SysPtrace {
    fn request(args: &[usize]) -> Result<PtraceRequest, SystemError> {
        PtraceRequest::try_from(args[0])
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

        let current = ProcessManager::current_pcb();

        // PTRACE_TRACEME：当前进程请求被其父跟踪，不走 find/check_attach 路径。
        if request == PtraceRequest::Traceme {
            ptrace::traceme_current()?;
            return Ok(0);
        }

        // 查找目标进程。
        let tracee = ProcessManager::find_task_by_vpid(pid).ok_or(SystemError::ESRCH)?;

        // ATTACH / SEIZE：建立关系，不做 check_attach。
        match request {
            PtraceRequest::Attach => {
                tracee.ptrace_attach(&current)?;
                return Ok(0);
            }
            PtraceRequest::Seize => {
                // SEIZE：addr 必须为 0，data 是选项位。
                if addr != 0 {
                    return Err(SystemError::EIO);
                }
                let options = ptrace::PtraceOptions::from_bits(data).ok_or(SystemError::EIO)?;
                tracee.ptrace_seize(&current, options)?;
                return Ok(0);
            }
            _ => {}
        }

        // 其余请求：要求 tracee 由 current 跟踪且处于可操作状态。
        tracee.ptrace_check_attach(request)?;

        let result: isize = match request {
            // DETACH：解除关系。data 是要注入的信号号（0=不注入）。
            PtraceRequest::Detach => {
                let signal = decode_injected_signal(data);
                tracee.ptrace_detach(signal)?
            }
            // KILL：直接发送 SIGKILL。
            PtraceRequest::Kill => {
                let mut info = SigInfo::new(
                    Signal::SIGKILL,
                    0,
                    SigCode::Kernel,
                    SigType::Kill {
                        pid: RawPid(0),
                        uid: 0,
                    },
                );
                Signal::SIGKILL.send_signal_info_to_pcb(
                    Some(&mut info),
                    tracee.clone(),
                    PidType::PID,
                )?;
                0
            }
            // INTERRUPT：让运行中的 SEIZED tracee 进入 ptrace-stop。
            PtraceRequest::Interrupt => {
                tracee.ptrace_interrupt()?;
                0
            }
            // LISTEN：让处于 PTRACE_EVENT_STOP 的 tracee 脱离 ptrace-stop 但保持 stopped。
            PtraceRequest::Listen => {
                tracee.ptrace_listen()?;
                0
            }
            // CONT / SYSCALL / SINGLESTEP / SYSEMU / SYSEMU_SINGLESTEP：恢复 tracee。
            PtraceRequest::Cont
            | PtraceRequest::Syscall
            | PtraceRequest::Singlestep
            | PtraceRequest::Sysemu
            | PtraceRequest::SysemuSinglestep => {
                tracee.ptrace_resume(request, decode_injected_signal(data))?
            }
            // SETOPTIONS：设置 ptrace 选项（strace 设置 TRACESYSGOOD 等）。
            PtraceRequest::Setoptions => {
                let opts = ptrace::PtraceOptions::from_bits(data).ok_or(SystemError::EINVAL)?;
                tracee.set_ptrace_options(opts)?;
                0
            }
            // GETEVENTMSG：读最近一次 event message。
            PtraceRequest::Geteventmsg => {
                // 把 event_message 写入用户态 data 指向的 unsigned long。
                let msg = tracee.ptrace_get_event_message();
                ptrace_store_word_to_user(data, &msg)?;
                0
            }
            // GETREGS / SETREGS：读写 x86_64 用户寄存器。
            #[cfg(target_arch = "x86_64")]
            PtraceRequest::Getregs => {
                let regs = tracee.tracee_user_regs();
                copy_to_user(data, &regs)?;
                0
            }
            #[cfg(target_arch = "x86_64")]
            PtraceRequest::Setregs => {
                let regs: ptrace::UserRegsStruct = copy_from_user(data)?;
                tracee.write_tracee_user_regs(&regs)?;
                0
            }
            // PEEKUSER / POKEUSER：按偏移读写 user_regs_struct。
            #[cfg(target_arch = "x86_64")]
            PtraceRequest::Peekuser => {
                // PEEKUSER 用 data 作为存放结果的用户地址（PEEK* 语义）。
                let val = tracee.ptrace_peek_user(addr)?;
                ptrace_store_word_to_user(data, &val)?;
                0
            }
            #[cfg(target_arch = "x86_64")]
            PtraceRequest::Pokeuser => {
                tracee.ptrace_poke_user(addr, data)?;
                0
            }
            // GETSIGINFO：读 last_siginfo，转为 siginfo_t 给用户。
            PtraceRequest::Getsiginfo => {
                let info = tracee.ptrace_get_siginfo()?;
                let posix = info.convert_to_posix_siginfo();
                copy_to_user(data, &posix)?;
                0
            }
            // SETSIGINFO：从用户 siginfo_t 整体更新 last_siginfo。
            PtraceRequest::Setsiginfo => {
                let mut posix: PosixSigInfo = PosixSigInfo::default();
                unsafe {
                    crate::syscall::user_access::read_one_from_user_protected(
                        VirtAddr::new(data),
                        &mut posix,
                    )?
                };
                let info = SigInfo::from_posix(&posix);
                tracee.ptrace_set_siginfo(info)?;
                0
            }
            // GETSIGMASK：addr 必须为 sizeof(sigset)。
            PtraceRequest::Getsigmask => {
                if addr != core::mem::size_of::<u64>() {
                    return Err(SystemError::EINVAL);
                }
                let mask = tracee.ptrace_get_sigmask();
                let bits: usize = mask.bits() as usize;
                ptrace_store_word_to_user(data, &bits)?;
                0
            }
            PtraceRequest::Setsigmask => {
                if addr != core::mem::size_of::<u64>() {
                    return Err(SystemError::EINVAL);
                }
                let bits = copy_from_user::<u64>(data)?;
                let mask = SigSet::from_bits_truncate(bits);
                tracee.ptrace_set_sigmask(mask);
                0
            }
            // GETREGSET / SETREGSET：
            // addr = NT_* note type；data = 用户态 struct iovec 指针。
            // 成功后把（min 截断后的）实际长度写回 iov.iov_len
            #[cfg(target_arch = "x86_64")]
            PtraceRequest::Getregset => {
                if addr != ptrace::NT_PRSTATUS as usize {
                    return Err(SystemError::EINVAL);
                }
                let (iov_base, iov_len) = read_iovec(data)?;
                let regs = tracee.tracee_user_regs();
                let len = iov_len.min(core::mem::size_of::<ptrace::UserRegsStruct>());
                // 走异常表保护，坏 iov_base 返回 EFAULT 而非 panic。
                let regs_bytes: &[u8] = unsafe {
                    core::slice::from_raw_parts(
                        &regs as *const ptrace::UserRegsStruct as *const u8,
                        core::mem::size_of::<ptrace::UserRegsStruct>(),
                    )
                };
                unsafe {
                    crate::syscall::user_access::copy_to_user_protected(
                        VirtAddr::new(iov_base),
                        &regs_bytes[..len],
                    )?;
                }
                write_iovec_len(data, len)?;
                0
            }
            #[cfg(target_arch = "x86_64")]
            PtraceRequest::Setregset => {
                if addr != ptrace::NT_PRSTATUS as usize {
                    return Err(SystemError::EINVAL);
                }
                let (iov_base, iov_len) = read_iovec(data)?;
                // iov_len 必须是寄存器字（8 字节）的整数倍，否则 EINVAL
                if iov_len % core::mem::size_of::<u64>() != 0 {
                    return Err(SystemError::EINVAL);
                }
                let len = iov_len.min(core::mem::size_of::<ptrace::UserRegsStruct>());
                // 读出当前寄存器，仅覆盖 iov 提供的前 len 字节，未覆盖字段（如 cs/ss）原样保留。
                let mut regs = tracee.tracee_user_regs();
                let regs_bytes: &mut [u8] = unsafe {
                    core::slice::from_raw_parts_mut(
                        &mut regs as *mut ptrace::UserRegsStruct as *mut u8,
                        core::mem::size_of::<ptrace::UserRegsStruct>(),
                    )
                };
                unsafe {
                    crate::syscall::user_access::copy_from_user_protected(
                        &mut regs_bytes[..len],
                        VirtAddr::new(iov_base),
                    )?;
                }
                tracee.write_tracee_user_regs(&regs)?;
                write_iovec_len(data, len)?;
                0
            }
            // GET_SYSCALL_INFO：读最近 syscall-stop 的 op/nr/args。
            // addr 是输出指针，data 是用户缓冲区大小。
            #[cfg(target_arch = "x86_64")]
            PtraceRequest::Getsyscallinfo => {
                let user_size = data;
                let info = tracee.ptrace_get_syscall_info(user_size)?;
                let actual: usize = match info.op {
                    ptrace::PtraceSyscallInfoOp::None => {
                        core::mem::offset_of!(ptrace::PtraceSyscallInfo, arch)
                            + core::mem::size_of::<u32>()
                    }
                    ptrace::PtraceSyscallInfoOp::Entry => 80,
                    ptrace::PtraceSyscallInfoOp::Exit => 33,
                    ptrace::PtraceSyscallInfoOp::Seccomp => 84,
                };
                // 截断到用户缓冲区大小，避免溢出。
                let write_size = actual.min(user_size);
                let info_bytes: &[u8] = unsafe {
                    core::slice::from_raw_parts(
                        &info as *const ptrace::PtraceSyscallInfo as *const u8,
                        actual,
                    )
                };
                unsafe {
                    crate::syscall::user_access::copy_to_user_protected(
                        VirtAddr::new(addr),
                        &info_bytes[..write_size],
                    )?;
                }
                actual as isize
            }
            // PEEKDATA/PEEKTEXT/POKEDATA/POKETEXT：读写 tracee 用户内存。
            PtraceRequest::Peektext | PtraceRequest::Peekdata => {
                let val = tracee.ptrace_peek_data(addr)?;
                ptrace_store_word_to_user(data, &val)?;
                0
            }
            PtraceRequest::Poketext | PtraceRequest::Pokedata => {
                tracee.ptrace_poke_data(addr, data)?;
                0
            }
            _ => {
                log::debug!("ptrace: request {:?} not yet implemented", request);
                return Err(SystemError::EIO);
            }
        };

        Ok(result as usize)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("request", format!("{:#x}", args[0])),
            FormattedSyscallParam::new("pid", format!("{:#x}", args[1] as i32)),
            FormattedSyscallParam::new("addr", format!("{:#x}", args[2])),
            FormattedSyscallParam::new("data", format!("{:#x}", args[3])),
        ]
    }
}

/// 解析 ATTACH/CONT/DETACH 等 request 的 data 参数为要注入的信号。
fn decode_injected_signal(data: usize) -> Option<Signal> {
    if data == 0 {
        return None;
    }
    if data > MAX_SIG_NUM {
        return Some(Signal::INVALID);
    }
    let sig = Signal::from(data as i32);
    if sig == Signal::INVALID {
        return Some(Signal::INVALID);
    }
    Some(sig)
}

/// 读用户态 iovec {base, len}（GETREGSET/SETREGSET 用）。返回 (base, len)。
fn read_iovec(addr: usize) -> Result<(usize, usize), SystemError> {
    // iovec = { void *iov_base; size_t iov_len; }
    let base = copy_from_user::<usize>(addr)?;
    let len = copy_from_user::<usize>(addr + core::mem::size_of::<usize>())?;
    Ok((base, len))
}

/// 写回 iovec 的 iov_len（GETREGSET 返回实际长度）。走异常表保护。
fn write_iovec_len(addr: usize, len: usize) -> Result<(), SystemError> {
    ptrace_store_word_to_user(addr + core::mem::size_of::<usize>(), &len)
}

/// put_user：把一个 Copy 值写入用户态地址（PEEKUSER/GETEVENTMSG/GETSIGMASK 等结果存放）
/// 走异常表保护，坏指针返回 EFAULT 而非 panic。
fn ptrace_store_word_to_user<T: Copy>(addr: usize, value: &T) -> Result<(), SystemError> {
    unsafe { crate::syscall::user_access::write_one_to_user_protected(VirtAddr::new(addr), value) }
}

/// 从用户态拷贝一个 Copy 结构（SETREGS/SETSIGINFO/SETSIGMASK/read_iovec 等）。
fn copy_from_user<T: Copy + Default>(addr: usize) -> Result<T, SystemError> {
    let mut dst: T = T::default();
    unsafe {
        crate::syscall::user_access::read_one_from_user_protected(VirtAddr::new(addr), &mut dst)?
    };
    Ok(dst)
}

/// 拷贝一个 Copy 结构到用户态（GETREGS/GETSIGINFO 等）。走异常表保护。
fn copy_to_user<T: Copy>(addr: usize, value: &T) -> Result<(), SystemError> {
    unsafe { crate::syscall::user_access::write_one_to_user_protected(VirtAddr::new(addr), value) }
}

syscall_table_macros::declare_syscall!(SYS_PTRACE, SysPtrace);
