use alloc::string::ToString;
use alloc::vec::Vec;
use core::ffi::c_int;
use core::mem::size_of;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_RT_SIGQUEUEINFO;
use crate::ipc::signal_types::{
    PosixSigInfo, SigCode, SigInfo, SigType, SIG_SPECIFIC_SICODES_MASK,
};
use crate::ipc::syscall::sys_kill::check_signal_permission_pcb_with_sig;
use crate::process::pid::PidType;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferReader;
use crate::{arch::ipc::signal::Signal, process::ProcessManager, process::RawPid};
use system_error::SystemError;

const NSIGPOLL: i32 = 6;
const NSIGILL: i32 = 11;
const NSIGFPE: i32 = 15;
const NSIGSEGV: i32 = 10;
const NSIGBUS: i32 = 5;
const NSIGTRAP: i32 = 6;
const NSIGSYS: i32 = 2;

fn is_positive_sig_specific_code(si_code: i32) -> bool {
    si_code > SigCode::User.as_i32() && si_code < SigCode::Kernel.as_i32()
}

fn is_fault_layout_signal(signal: Signal) -> bool {
    matches!(
        signal,
        Signal::SIGILL | Signal::SIGFPE | Signal::SIGSEGV | Signal::SIGBUS | Signal::SIGTRAP
    )
}

fn signal_specific_code_limit(signal: Signal) -> Option<i32> {
    match signal {
        Signal::SIGILL => Some(NSIGILL),
        Signal::SIGFPE => Some(NSIGFPE),
        Signal::SIGSEGV => Some(NSIGSEGV),
        Signal::SIGBUS => Some(NSIGBUS),
        Signal::SIGTRAP => Some(NSIGTRAP),
        Signal::SIGSYS => Some(NSIGSYS),
        Signal::SIGIO_OR_POLL => Some(NSIGPOLL),
        _ => None,
    }
}

fn signal_has_specific_si_codes(signal: Signal) -> bool {
    SIG_SPECIFIC_SICODES_MASK.contains(Signal::into_sigset(signal))
}

fn raw_siginfo_pid(pid: i32) -> RawPid {
    RawPid::new(pid as usize)
}

pub(crate) fn sig_type_from_user_siginfo(
    signal: Signal,
    code_enum: SigCode,
    user_info: &PosixSigInfo,
) -> SigType {
    match code_enum {
        SigCode::Timer => {
            let timer = unsafe { user_info._sifields._timer };
            SigType::PosixTimer {
                timerid: timer.si_tid,
                overrun: timer.si_overrun,
                sigval: timer.si_sigval,
            }
        }
        SigCode::SigIO => {
            let sigpoll = unsafe { user_info._sifields._sigpoll };
            SigType::SigPoll {
                fd: sigpoll.si_fd,
                band: sigpoll.si_band,
            }
        }
        SigCode::Raw(code) if is_positive_sig_specific_code(code) => {
            let specific_limit = signal_specific_code_limit(signal);
            if is_fault_layout_signal(signal) && specific_limit.is_some_and(|limit| code <= limit) {
                let fault = unsafe { user_info._sifields._sigfault };
                SigType::Fault {
                    addr: fault.si_addr,
                    addr_lsb: fault.si_addr_lsb,
                }
            } else if signal == Signal::SIGSYS && specific_limit.is_some_and(|limit| code <= limit)
            {
                let sigsys = unsafe { user_info._sifields._sigsys };
                SigType::SigSys {
                    call_addr: sigsys._call_addr,
                    syscall: sigsys._syscall,
                    arch: sigsys._arch,
                }
            } else if code <= NSIGPOLL
                && (signal == Signal::SIGIO_OR_POLL || !signal_has_specific_si_codes(signal))
            {
                let sigpoll = unsafe { user_info._sifields._sigpoll };
                SigType::SigPoll {
                    fd: sigpoll.si_fd,
                    band: sigpoll.si_band,
                }
            } else {
                let kill = unsafe { user_info._sifields._kill };
                SigType::Kill {
                    pid: raw_siginfo_pid(kill.si_pid),
                    uid: kill.si_uid,
                }
            }
        }
        SigCode::Raw(code) if code < 0 => {
            let rt = unsafe { user_info._sifields._rt };
            SigType::Rt {
                pid: raw_siginfo_pid(rt.si_pid),
                uid: rt.si_uid,
                sigval: rt.si_sigval,
            }
        }
        SigCode::Queue | SigCode::Mesgq | SigCode::AsyncIO | SigCode::Tkill => {
            let rt = unsafe { user_info._sifields._rt };
            SigType::Rt {
                pid: raw_siginfo_pid(rt.si_pid),
                uid: rt.si_uid,
                sigval: rt.si_sigval,
            }
        }
        SigCode::PollIn
        | SigCode::PollOut
        | SigCode::PollMsg
        | SigCode::PollErr
        | SigCode::PollPri
        | SigCode::PollHup => {
            let sigpoll = unsafe { user_info._sifields._sigpoll };
            SigType::SigPoll {
                fd: sigpoll.si_fd,
                band: sigpoll.si_band,
            }
        }
        _ => {
            let kill = unsafe { user_info._sifields._kill };
            SigType::Kill {
                pid: raw_siginfo_pid(kill.si_pid),
                uid: kill.si_uid,
            }
        }
    }
}

/// rt_sigqueueinfo 系统调用（最小兼容实现）
///
/// 语义上与 kill(pid, sig) 类似，但允许用户态携带一个 siginfo_t。
/// 参考 Linux 6.6：
/// - 从用户态拷贝 siginfo（并强制以参数 sig 作为 si_signo）
/// - 若向“非自身 pid”发送，禁止伪造内核/kill/tkill 的 si_code：
///   `(si_code >= 0 || si_code == SI_TKILL) && current_pid != pid` => EPERM
/// - 将 si_errno 等字段随信号投递给目标。
struct SysRtSigqueueinfoHandle;

impl SysRtSigqueueinfoHandle {
    #[inline(always)]
    fn pid(args: &[usize]) -> i32 {
        args[0] as i32
    }

    #[inline(always)]
    fn sig(args: &[usize]) -> c_int {
        args[1] as c_int
    }

    #[inline(always)]
    fn _uinfo(args: &[usize]) -> usize {
        args[2]
    }
}

impl Syscall for SysRtSigqueueinfoHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = Self::pid(args);
        let sig = Self::sig(args);
        let uinfo = Self::_uinfo(args) as *const PosixSigInfo;

        if pid <= 0 {
            return Err(SystemError::EINVAL);
        }

        if uinfo.is_null() {
            return Err(SystemError::EFAULT);
        }

        // sig==0 探测模式：只做存在性与权限检查（与 kill(2) 语义一致）
        if sig == 0 {
            let target_pid = RawPid::from(pid as usize);
            let target = ProcessManager::find_task_by_vpid(target_pid).ok_or(SystemError::ESRCH)?;
            // 传入 Signal::INVALID/0 在权限检查里无特殊含义，这里用 None 即可
            check_signal_permission_pcb_with_sig(&target, None)?;
            return Ok(0);
        }

        let signal = Signal::from(sig);
        if signal == Signal::INVALID {
            return Err(SystemError::EINVAL);
        }

        // 从用户空间读取 siginfo_t
        let reader = UserBufferReader::new(uinfo, size_of::<PosixSigInfo>(), true)?;
        let buffer = reader.buffer_protected(0)?;
        let user_info = buffer.read_one::<PosixSigInfo>(0)?;

        let target_pid = RawPid::from(pid as usize);
        let current_pcb = ProcessManager::current_pcb();
        let current_pid = current_pcb.raw_pid();

        // Linux 6.6: do_rt_sigqueueinfo 权限校验
        let si_code = user_info.si_code;
        if (si_code >= 0 || si_code == SigCode::Tkill.as_i32()) && current_pid != target_pid {
            return Err(SystemError::EPERM);
        }

        let code_enum = SigCode::try_from_i32(si_code).unwrap_or(SigCode::Raw(si_code));
        let sig_type = sig_type_from_user_siginfo(signal, code_enum, &user_info);

        let mut info = SigInfo::new(signal, user_info.si_errno, code_enum, sig_type);

        // 查找目标进程并检查权限
        let target = ProcessManager::find_task_by_vpid(target_pid).ok_or(SystemError::ESRCH)?;
        check_signal_permission_pcb_with_sig(&target, Some(signal))?;

        // rt_sigqueueinfo 发送进程级信号，使用 PidType::TGID
        signal
            .send_signal_info_to_pcb(Some(&mut info), target, PidType::TGID)
            .map(|x| x as usize)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", Self::pid(args).to_string()),
            FormattedSyscallParam::new("sig", Self::sig(args).to_string()),
            FormattedSyscallParam::new("uinfo", format!("{:#x}", Self::_uinfo(args))),
        ]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_RT_SIGQUEUEINFO, SysRtSigqueueinfoHandle);
