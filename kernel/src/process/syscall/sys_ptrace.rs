//! ptrace system call dispatch.
//!
//! This file only performs request parsing, argument validation, target process lookup,
//! and the `ptrace_check_attach` pre-check; the real logic lives in the `process::ptrace` PCB methods.

use crate::{
    arch::{
        interrupt::TrapFrame,
        ipc::signal::{SigSet, Signal, MAX_SIG_NUM},
        syscall::nr::SYS_PTRACE,
    },
    ipc::signal_types::{SigCode, SigInfo, SigType},
    mm::{access_ok, VirtAddr},
    process::{
        pid::PidType,
        ptrace::{self, PtraceRequest, PtraceRequestGuard},
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

        // PTRACE_TRACEME: the current process asks to be traced by its parent; this skips the find/check_attach path.
        if request == PtraceRequest::Traceme {
            ptrace::traceme_current()?;
            return Ok(0);
        }

        // Find the target process.
        let tracee = ProcessManager::find_task_by_vpid(pid).ok_or(SystemError::ESRCH)?;

        // ATTACH / SEIZE: establish the relationship without check_attach.
        match request {
            PtraceRequest::Attach => {
                tracee.ptrace_attach(&current)?;
                return Ok(0);
            }
            PtraceRequest::Seize => {
                // SEIZE: addr must be 0 and data holds the option bits.
                if addr != 0 {
                    return Err(SystemError::EIO);
                }
                let options = ptrace::PtraceOptions::from_bits(data).ok_or(SystemError::EIO)?;
                tracee.ptrace_seize(&current, options)?;
                return Ok(0);
            }
            _ => {}
        }

        // Linux permits KILL/INTERRUPT without TASK_TRACED/freeze. Every other
        // request owns a generation-bound freeze until the guard is dropped or
        // consumed by a resume/detach/listen transition.
        let mut request_guard = if request == PtraceRequest::Kill {
            tracee.ptrace_check_non_frozen(&current)?;
            None
        } else if request == PtraceRequest::Interrupt {
            // Ownership is checked together with the pending-stop publication
            // in ptrace_interrupt(), under the relation lock.
            None
        } else {
            Some(PtraceRequestGuard::begin(tracee.clone(), current.clone())?)
        };

        let result: isize = match request {
            // DETACH: detach the tracee. data is the signal to inject (0 = none).
            PtraceRequest::Detach => {
                let signal = decode_injected_signal(data);
                request_guard
                    .take()
                    .ok_or(SystemError::ESRCH)?
                    .detach(signal)?
            }
            // KILL: send SIGKILL directly.
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
            // INTERRUPT: force a running SEIZED tracee into ptrace-stop.
            PtraceRequest::Interrupt => {
                tracee.ptrace_interrupt(&current)?;
                0
            }
            // LISTEN: take a tracee at PTRACE_EVENT_STOP out of ptrace-stop while keeping it stopped.
            PtraceRequest::Listen => request_guard.take().ok_or(SystemError::ESRCH)?.listen()?,
            // CONT / SYSCALL / SINGLESTEP / SYSEMU / SYSEMU_SINGLESTEP: resume the tracee.
            PtraceRequest::Cont
            | PtraceRequest::Syscall
            | PtraceRequest::Singlestep
            | PtraceRequest::Sysemu
            | PtraceRequest::SysemuSinglestep => request_guard
                .take()
                .ok_or(SystemError::ESRCH)?
                .resume(request, decode_injected_signal(data))?,
            // SETOPTIONS: set ptrace options (strace sets TRACESYSGOOD, etc.).
            PtraceRequest::Setoptions => {
                let opts = ptrace::PtraceOptions::from_bits(data).ok_or(SystemError::EINVAL)?;
                request_guard
                    .as_ref()
                    .ok_or(SystemError::ESRCH)?
                    .set_options(opts)?;
                0
            }
            // GETEVENTMSG: read the most recent event message.
            PtraceRequest::Geteventmsg => {
                // Write event_message to the unsigned long in user space pointed to by data.
                let msg = request_guard
                    .as_ref()
                    .ok_or(SystemError::ESRCH)?
                    .event_message();
                ptrace_store_word_to_user(data, &msg)?;
                0
            }
            // GETREGS / SETREGS: read/write the x86_64 user registers.
            #[cfg(target_arch = "x86_64")]
            PtraceRequest::Getregs => {
                let regs = request_guard
                    .as_ref()
                    .ok_or(SystemError::ESRCH)?
                    .get_regs()?;
                copy_to_user(data, &regs)?;
                0
            }
            #[cfg(target_arch = "x86_64")]
            PtraceRequest::Setregs => {
                let guard = request_guard.as_ref().ok_or(SystemError::ESRCH)?;
                let regs_len = core::mem::size_of::<ptrace::UserRegsStruct>();
                access_ok(VirtAddr::new(data), regs_len).map_err(|_| SystemError::EFAULT)?;
                // Linux's genregs_set() fetches and commits one machine word
                // at a time. Preserve that observable partial-update behavior
                // when a later user read or selector validation fails.
                for offset in (0..regs_len).step_by(core::mem::size_of::<u64>()) {
                    let word_addr = data.checked_add(offset).ok_or(SystemError::EFAULT)?;
                    let word = copy_from_user::<u64>(word_addr)?;
                    guard.poke_user(offset, word as usize)?;
                }
                0
            }
            // PEEKUSER / POKEUSER: read/write user_regs_struct by offset.
            #[cfg(target_arch = "x86_64")]
            PtraceRequest::Peekuser => {
                // PEEKUSER uses data as the user address where the result is stored (PEEK* semantics).
                let val = request_guard
                    .as_ref()
                    .ok_or(SystemError::ESRCH)?
                    .peek_user(addr)?;
                ptrace_store_word_to_user(data, &val)?;
                0
            }
            #[cfg(target_arch = "x86_64")]
            PtraceRequest::Pokeuser => {
                request_guard
                    .as_ref()
                    .ok_or(SystemError::ESRCH)?
                    .poke_user(addr, data)?;
                0
            }
            // GETSIGINFO: read last_siginfo and convert it to siginfo_t for the user.
            PtraceRequest::Getsiginfo => {
                let info = request_guard
                    .as_ref()
                    .ok_or(SystemError::ESRCH)?
                    .get_siginfo()?;
                let posix = info.convert_to_posix_siginfo();
                copy_to_user(data, &posix)?;
                0
            }
            // SETSIGINFO: update last_siginfo wholesale from the user siginfo_t.
            PtraceRequest::Setsiginfo => {
                let posix = unsafe {
                    crate::ipc::signal_types::copy_siginfo_from_user(VirtAddr::new(data), None)?
                };
                let info = SigInfo::from_posix(&posix);
                request_guard
                    .as_ref()
                    .ok_or(SystemError::ESRCH)?
                    .set_siginfo(info)?;
                0
            }
            // GETSIGMASK: addr must equal sizeof(sigset).
            PtraceRequest::Getsigmask => {
                if addr != core::mem::size_of::<u64>() {
                    return Err(SystemError::EINVAL);
                }
                let mask = request_guard
                    .as_ref()
                    .ok_or(SystemError::ESRCH)?
                    .get_sigmask();
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
                request_guard
                    .as_ref()
                    .ok_or(SystemError::ESRCH)?
                    .set_sigmask(mask);
                0
            }
            // GETREGSET / SETREGSET:
            // addr = NT_* note type; data = pointer to a user-space struct iovec.
            // On success, write the actual (min-truncated) length back to iov.iov_len
            #[cfg(target_arch = "x86_64")]
            PtraceRequest::Getregset => {
                if addr != ptrace::NT_PRSTATUS as usize {
                    return Err(SystemError::EINVAL);
                }
                let (iov_base, iov_len) = read_iovec(data)?;
                if iov_len % core::mem::size_of::<u64>() != 0 {
                    return Err(SystemError::EINVAL);
                }
                let regs = request_guard
                    .as_ref()
                    .ok_or(SystemError::ESRCH)?
                    .get_regs()?;
                let len = iov_len.min(core::mem::size_of::<ptrace::UserRegsStruct>());
                // Uses exception-table protection; a bad iov_base returns EFAULT rather than panicking.
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
                // iov_len must be a multiple of the register word size (8 bytes), otherwise EINVAL
                if iov_len % core::mem::size_of::<u64>() != 0 {
                    return Err(SystemError::EINVAL);
                }
                let len = iov_len.min(core::mem::size_of::<ptrace::UserRegsStruct>());
                let guard = request_guard.as_ref().ok_or(SystemError::ESRCH)?;
                access_ok(VirtAddr::new(iov_base), len).map_err(|_| SystemError::EFAULT)?;
                // copy_regset_from_user() reaches x86 genregs_set(), which
                // fetches and applies each word before moving to the next.
                for offset in (0..len).step_by(core::mem::size_of::<u64>()) {
                    let word_addr = iov_base.checked_add(offset).ok_or(SystemError::EFAULT)?;
                    let word = copy_from_user::<u64>(word_addr)?;
                    guard.poke_user(offset, word as usize)?;
                }
                write_iovec_len(data, len)?;
                0
            }
            // GET_SYSCALL_INFO: read the op/nr/args of the most recent syscall-stop.
            // Linux ABI: addr is the user buffer size and data is the output buffer pointer.
            #[cfg(target_arch = "x86_64")]
            PtraceRequest::Getsyscallinfo => {
                let user_size = addr;
                let info = request_guard
                    .as_ref()
                    .ok_or(SystemError::ESRCH)?
                    .syscall_info()?;
                let actual: usize = match info.op {
                    ptrace::PtraceSyscallInfoOp::None => {
                        core::mem::offset_of!(ptrace::PtraceSyscallInfo, data)
                    }
                    ptrace::PtraceSyscallInfoOp::Entry => 80,
                    ptrace::PtraceSyscallInfoOp::Exit => 33,
                    ptrace::PtraceSyscallInfoOp::Seccomp => 84,
                };
                // Truncate to the user buffer size to avoid overflow; the return value is the "number of bytes available".
                let write_size = actual.min(user_size);
                let info_bytes: &[u8] = unsafe {
                    core::slice::from_raw_parts(
                        &info as *const ptrace::PtraceSyscallInfo as *const u8,
                        actual,
                    )
                };
                unsafe {
                    crate::syscall::user_access::copy_to_user_protected(
                        VirtAddr::new(data),
                        &info_bytes[..write_size],
                    )?;
                }
                actual as isize
            }
            // PEEKDATA/PEEKTEXT/POKEDATA/POKETEXT: read/write the tracee's user memory.
            PtraceRequest::Peektext | PtraceRequest::Peekdata => {
                let val = request_guard
                    .as_ref()
                    .ok_or(SystemError::ESRCH)?
                    .peek_data(addr)?;
                ptrace_store_word_to_user(data, &val)?;
                0
            }
            PtraceRequest::Poketext | PtraceRequest::Pokedata => {
                request_guard
                    .as_ref()
                    .ok_or(SystemError::ESRCH)?
                    .poke_data(addr, data)?;
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

/// Decode the data argument of requests such as ATTACH/CONT/DETACH into the signal to inject.
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

/// Read a user-space iovec {base, len} (used by GETREGSET/SETREGSET). Returns (base, len).
fn read_iovec(addr: usize) -> Result<(usize, usize), SystemError> {
    // iovec = { void *iov_base; size_t iov_len; }
    let base = copy_from_user::<usize>(addr)?;
    let len = copy_from_user::<usize>(addr + core::mem::size_of::<usize>())?;
    Ok((base, len))
}

/// Write back the iovec's iov_len (GETREGSET returns the actual length). Uses exception-table protection.
fn write_iovec_len(addr: usize, len: usize) -> Result<(), SystemError> {
    ptrace_store_word_to_user(addr + core::mem::size_of::<usize>(), &len)
}

/// put_user: write a Copy value to a user-space address (result storage for PEEKUSER/GETEVENTMSG/GETSIGMASK, etc.)
/// Uses exception-table protection; a bad pointer returns EFAULT rather than panicking.
fn ptrace_store_word_to_user<T: Copy>(addr: usize, value: &T) -> Result<(), SystemError> {
    unsafe { crate::syscall::user_access::write_one_to_user_protected(VirtAddr::new(addr), value) }
}

/// Copy a Copy structure from user space (SETREGS/SETSIGINFO/SETSIGMASK/read_iovec, etc.).
fn copy_from_user<T: Copy + Default>(addr: usize) -> Result<T, SystemError> {
    let mut dst: T = T::default();
    unsafe {
        crate::syscall::user_access::read_one_from_user_protected(VirtAddr::new(addr), &mut dst)?
    };
    Ok(dst)
}

/// Copy a Copy structure to user space (GETREGS/GETSIGINFO, etc.). Uses exception-table protection.
fn copy_to_user<T: Copy>(addr: usize, value: &T) -> Result<(), SystemError> {
    unsafe { crate::syscall::user_access::write_one_to_user_protected(VirtAddr::new(addr), value) }
}

syscall_table_macros::declare_syscall!(SYS_PTRACE, SysPtrace);
