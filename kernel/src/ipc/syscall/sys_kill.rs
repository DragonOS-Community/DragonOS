use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ffi::c_int;

use crate::arch::interrupt::TrapFrame;
use crate::process::cred::CAPFlags;
use crate::process::pid::Pid;
use crate::process::ProcessControlBlock;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::{
    arch::{ipc::signal::Signal, syscall::nr::SYS_KILL},
    process::{ProcessManager, RawPid},
};
use log::warn;
use system_error::SystemError;

use crate::ipc::kill::{kill_all, kill_process, kill_process_group};

/// ### pid转换器，将输入的id转换成对应的pid或pgid
/// - 如果id < -1，则为pgid
/// - 如果id == -1，则为所有进程
/// - 如果id == 0，则为当前进程组
/// - 如果id > 0，则为pid
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PidConverter {
    All,
    Pid(Arc<Pid>),
    Pgid(Option<Arc<Pid>>),
}

impl PidConverter {
    /// ### 为 `wait` 和 `kill` 调用使用
    pub fn from_id(id: i32) -> Option<Self> {
        if id < -1 {
            let pgid = ProcessManager::find_vpid(RawPid::from(-id as usize));
            Some(PidConverter::Pgid(pgid))
        } else if id == -1 {
            Some(PidConverter::All)
        } else if id == 0 {
            let pgid = ProcessManager::current_pcb().task_pgrp().unwrap();
            Some(PidConverter::Pgid(Some(pgid)))
        } else {
            let pid = ProcessManager::find_vpid(RawPid::from(id as usize))?;
            Some(PidConverter::Pid(pid))
        }
    }

    /// ### 为 `waitid` 使用：which/upid 已在封装层基本校验
    /// 约定：which: 0=P_ALL, 1=P_PID(id>0), 2=P_PGID(id>=0; 0=当前组)
    pub fn from_waitid(which: u32, upid: i32) -> Option<Self> {
        match which {
            0 => Some(PidConverter::All),
            1 => {
                if upid <= 0 {
                    return None;
                }
                Self::from_id(upid)
            }
            2 => {
                if upid < 0 {
                    return None;
                }
                // P_PGID: upid==0 -> 当前进程组；>0 -> 指定pgid
                // from_id: id< -1 为 pgid，因此这里将正 pgid 映射为负数传入
                if upid == 0 {
                    Self::from_id(0)
                } else {
                    Self::from_id(-upid)
                }
            }
            _ => None,
        }
    }
}

/// Check if the current process has permission to send a signal to the target process.
///
/// # Arguments
/// * `target` - The target PCB
/// * `sig` - The signal to be sent (optional, for SIGCONT special handling)
///
/// # Returns
/// * `Ok(())` - Permission check passed
/// * `Err(SystemError::EPERM)` - Permission denied
///
/// # POSIX Requirements
/// The permission check follows POSIX requirements:
/// - CAP_KILL capability can signal any process
/// - Root (euid == 0) can signal any process
/// - Sender's real or effective UID must match target's real or saved set-user-ID
/// - SIGCONT can be sent to any process in the same session
///
/// 参考: https://man7.org/linux/man-pages/man2/kill.2.html
pub fn check_signal_permission_pcb_with_sig(
    target: &Arc<ProcessControlBlock>,
    sig: Option<Signal>,
) -> Result<(), SystemError> {
    let current_pcb = ProcessManager::current_pcb();
    let current_cred = current_pcb.cred();
    let target_cred = target.cred();

    // CAP_KILL allows sending signal to any process
    if current_cred.has_capability(CAPFlags::CAP_KILL) {
        return Ok(());
    }

    // Root can signal any process
    if current_cred.euid.data() == 0 {
        return Ok(());
    }

    // Check if sender's UID matches target's UID or saved UID
    if current_cred.euid == target_cred.uid
        || current_cred.euid == target_cred.suid
        || current_cred.uid == target_cred.uid
        || current_cred.uid == target_cred.suid
    {
        return Ok(());
    }

    // SIGCONT can be sent to any process in the same session
    // 参考: https://man7.org/linux/man-pages/man2/kill.2.html
    if let Some(signal) = sig {
        if signal == Signal::SIGCONT {
            // 检查是否在同一会话中
            let current_session = current_pcb.task_session();
            let target_session = target.task_session();
            if let (Some(cs), Some(ts)) = (current_session, target_session) {
                if Arc::ptr_eq(&cs, &ts) {
                    return Ok(());
                }
            }
        }
    }

    Err(SystemError::EPERM)
}

/// Check if the current process has permission to send a signal to the target process.
/// (不带信号参数的兼容版本)
pub fn check_signal_permission_pcb(target: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
    check_signal_permission_pcb_with_sig(target, None)
}

/// Check if the current process has permission to send a signal to the target process.
fn check_signal_permission(target_pid: RawPid) -> Result<(), SystemError> {
    let target = ProcessManager::find_task_by_vpid(target_pid).ok_or(SystemError::ESRCH)?;
    check_signal_permission_pcb(&target)
}

/// Handle signal 0 (null signal) which is used to check process existence and permissions.
///
/// According to POSIX, if sig is 0, no signal is sent, but error checking is still performed.
/// See: https://pubs.opengroup.org/onlinepubs/9699919799/functions/kill.html
///
/// # Arguments
/// * `converter` - The PID converter indicating the target (process, group, or all)
///
/// # Returns
/// * `Ok(0)` - Target exists and permission check passed
/// * `Err(SystemError::ESRCH)` - Target does not exist
/// * `Err(SystemError::EPERM)` - Permission denied
fn handle_null_signal(converter: &PidConverter) -> Result<usize, SystemError> {
    match converter {
        PidConverter::Pid(pid) => {
            // Check existence and permissions for a specific process
            check_signal_permission(pid.pid_vnr())?;
            Ok(0)
        }
        PidConverter::Pgid(pgid) => {
            // For process groups, verify the group exists
            // A more complete implementation could check all processes in the group
            pgid.as_ref().ok_or(SystemError::ESRCH)?;
            Ok(0)
        }
        PidConverter::All => {
            // Signal 0 to all processes: just verify the syscall is valid
            Ok(0)
        }
    }
}

pub struct SysKillHandle;

impl SysKillHandle {
    #[inline(always)]
    fn pid(args: &[usize]) -> i32 {
        // 第一个参数是id
        args[0] as i32
    }
    #[inline(always)]
    fn sig(args: &[usize]) -> c_int {
        // 第二个参数是信号值
        args[1] as c_int
    }
}

impl Syscall for SysKillHandle {
    fn num_args(&self) -> usize {
        2
    }
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let id = Self::pid(args);
        let sig_c_int = Self::sig(args);

        let converter = PidConverter::from_id(id).ok_or(SystemError::ESRCH)?;

        // Handle null signal (signal 0) - used for existence and permission checks
        if sig_c_int == 0 {
            return handle_null_signal(&converter);
        }

        let sig = Signal::from(sig_c_int);
        if sig == Signal::INVALID {
            warn!(
                "Failed to convert signal number {} to Signal enum",
                sig_c_int
            );
            return Err(SystemError::EINVAL);
        }

        match converter {
            PidConverter::Pid(pid) => kill_process(pid.pid_vnr(), sig),
            PidConverter::Pgid(pgid) => kill_process_group(&pgid.ok_or(SystemError::ESRCH)?, sig),
            PidConverter::All => kill_all(sig),
        }
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", Self::pid(args).to_string()),
            FormattedSyscallParam::new("sig", Self::sig(args).to_string()),
        ]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_KILL, SysKillHandle);
