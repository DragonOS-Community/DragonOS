use alloc::string::ToString;
use alloc::vec::Vec;
use core::ffi::c_int;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, ipc::signal::Signal, syscall::nr::SYS_TKILL},
    ipc::signal_types::{SigCode, SigInfo, SigType},
    process::{namespace::user_namespace::from_kuid_munged, pid::PidType, ProcessManager, RawPid},
    syscall::table::{FormattedSyscallParam, Syscall},
};

use super::sys_kill::check_signal_permission_pcb_with_sig;

pub struct SysTkillHandle;

impl SysTkillHandle {
    fn tid(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn sig(args: &[usize]) -> c_int {
        args[1] as c_int
    }
}

impl Syscall for SysTkillHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let tid = Self::tid(args);
        if tid <= 0 {
            return Err(SystemError::EINVAL);
        }
        do_tkill(0, tid, Self::sig(args))
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("tid", Self::tid(args).to_string()),
            FormattedSyscallParam::new("sig", Self::sig(args).to_string()),
        ]
    }
}

/// tkill/tgkill synthesize their source identity; queued callers provide it.
pub fn do_tkill(tgid: i32, tid: i32, sig: c_int) -> Result<usize, SystemError> {
    let current = ProcessManager::current_pcb();
    let cred = current.cred();
    let mut info = SigInfo::new(
        Signal::from(sig),
        0,
        SigCode::Tkill,
        SigType::Kill {
            pid: current.task_tgid_vnr().unwrap_or(RawPid::from(0)),
            uid: from_kuid_munged(&cred.user_ns, cred.uid),
        },
    );
    do_send_specific(tgid, tid, sig, &mut info)
}

/// Shared thread-directed delivery, matching Linux do_send_specific().
/// tgid=0 is tkill's explicit exemption from thread-group matching.
pub(super) fn do_send_specific(
    tgid: i32,
    tid: i32,
    sig: c_int,
    info: &mut SigInfo,
) -> Result<usize, SystemError> {
    let target =
        ProcessManager::find_task_by_vpid(RawPid::from(tid as usize)).ok_or(SystemError::ESRCH)?;
    if tgid > 0 && target.task_tgid_vnr() != Some(RawPid::from(tgid as usize)) {
        return Err(SystemError::ESRCH);
    }
    let signal = Signal::from(sig);
    if sig != 0 && signal == Signal::INVALID {
        return Err(SystemError::EINVAL);
    }
    check_signal_permission_pcb_with_sig(&target, (sig != 0).then_some(signal))?;
    if sig == 0 {
        return Ok(0);
    }
    // Only a target disappearing after lookup is tolerated, not lookup ESRCH.
    match signal.send_user_signal_info_to_pcb(info, target, PidType::PID) {
        Err(SystemError::ESRCH) => Ok(0),
        result => result.map(|_| 0),
    }
}

syscall_table_macros::declare_syscall!(SYS_TKILL, SysTkillHandle);
