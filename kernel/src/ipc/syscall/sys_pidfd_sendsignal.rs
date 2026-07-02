use crate::arch::ipc::signal::Signal;
use crate::ipc::signal_types::{PosixSigInfo, SigCode};
use crate::ipc::signal_types::{SigInfo, SigType};
use crate::ipc::syscall::sys_kill::check_signal_permission_pcb_with_sig;
use crate::ipc::syscall::sys_rt_sigqueueinfo::sig_type_from_user_siginfo;
use crate::process::pid::{Pid, PidType};
use alloc::{string::ToString, sync::Arc, vec::Vec};
use core::ffi::c_int;
use core::mem::size_of;

use crate::arch::interrupt::TrapFrame;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferReader;
use crate::{arch::syscall::nr::SYS_PIDFD_SEND_SIGNAL, process::ProcessManager};
use system_error::SystemError;

pub struct SysPidfdSendSignalHandle;

fn pidfd_pidns_accessible(pid: &Arc<Pid>) -> bool {
    let active = ProcessManager::current_pcb().active_pid_ns();
    let mut cursor = Some(pid.ns_of_pid());
    while let Some(ns) = cursor {
        if Arc::ptr_eq(&ns, &active) {
            return true;
        }
        cursor = ns.parent();
    }
    false
}

impl SysPidfdSendSignalHandle {
    #[inline(always)]
    fn pidfd(args: &[usize]) -> i32 {
        args[0] as i32
    }
    #[inline(always)]
    fn sig(args: &[usize]) -> c_int {
        args[1] as c_int
    }
    #[inline(always)]
    fn siginfo(args: &[usize]) -> *const PosixSigInfo {
        args[2] as *const PosixSigInfo
    }
    #[inline(always)]
    fn flags(args: &[usize]) -> usize {
        args[3]
    }
}

impl Syscall for SysPidfdSendSignalHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pidfd = Self::pidfd(args);
        let sig_c_int = Self::sig(args);
        let sig_info = Self::siginfo(args);
        let flags = Self::flags(args);

        if flags != 0 {
            return Err(SystemError::EINVAL);
        }

        // TODO: 完整的支持此系统调用
        let target_pid = ProcessManager::current_pcb().pidfd_target_from_fd(pidfd)?;
        let target = target_pid.task(PidType::TGID).ok_or(SystemError::ESRCH)?;

        if !pidfd_pidns_accessible(&target_pid.pid()) {
            return Err(SystemError::EINVAL);
        }

        if sig_c_int == 0 {
            check_signal_permission_pcb_with_sig(&target, None)?;
            // log::warn!("Pidfd_Send_Signal: Send empty sig(0)");
            // 这里的信号是 0, 是空信号值, 其他的信号处理是怎样的不清楚, 但是这里应该直接返回成功, 因为 0 是空信号
            return Ok(0);
        }

        let sig = Signal::from(sig_c_int);
        if sig == Signal::INVALID {
            return Err(SystemError::EINVAL);
        }

        let mut info = if sig_info.is_null() {
            let current_pcb = ProcessManager::current_pcb();
            let sender_pid = current_pcb.raw_pid();
            let sender_uid = current_pcb.cred().uid.data() as u32;
            SigInfo::new(
                sig,
                0,
                SigCode::User,
                SigType::Kill {
                    pid: sender_pid,
                    uid: sender_uid,
                },
            )
        } else {
            let reader = UserBufferReader::new(sig_info, size_of::<PosixSigInfo>(), true)?;
            let buffer = reader.buffer_protected(0)?;
            let user_info = buffer.read_one::<PosixSigInfo>(0)?;
            if user_info.si_signo != sig_c_int {
                return Err(SystemError::EINVAL);
            }

            let current_pid = ProcessManager::current_pcb().pid();
            let si_code = user_info.si_code;
            if (si_code >= 0 || si_code == SigCode::Tkill.as_i32())
                && !Arc::ptr_eq(&current_pid, &target_pid.pid())
            {
                return Err(SystemError::EPERM);
            }

            let code_enum = SigCode::try_from_i32(si_code).unwrap_or(SigCode::Raw(si_code));
            let sig_type = sig_type_from_user_siginfo(sig, code_enum, &user_info);
            SigInfo::new(sig, user_info.si_errno, code_enum, sig_type)
        };

        check_signal_permission_pcb_with_sig(&target, Some(sig))?;

        let ret = sig
            .send_signal_info_to_pcb(Some(&mut info), target, PidType::TGID)
            .map(|x| x as usize);

        ret
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pidfd", Self::pidfd(args).to_string()),
            FormattedSyscallParam::new("sig", Self::sig(args).to_string()),
            FormattedSyscallParam::new("siginfo", format!("{:#x}", Self::siginfo(args) as usize)),
            FormattedSyscallParam::new("options", format!("{:#x}", Self::flags(args))),
        ]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_PIDFD_SEND_SIGNAL, SysPidfdSendSignalHandle);
