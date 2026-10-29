use alloc::string::ToString;
use alloc::vec::Vec;
use core::ffi::c_int;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_RT_TGSIGQUEUEINFO;
use crate::ipc::signal_types::{copy_siginfo_from_user, SigCode, SigInfo};
use crate::ipc::syscall::sys_tkill::do_send_specific;
use crate::mm::VirtAddr;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use system_error::SystemError;

/// Queue userspace siginfo to one exact thread in the caller's PID namespace.
struct SysRtTgsigqueueinfoHandle;

impl SysRtTgsigqueueinfoHandle {
    #[inline(always)]
    fn tgid(args: &[usize]) -> i32 {
        args[0] as i32
    }

    #[inline(always)]
    fn tid(args: &[usize]) -> i32 {
        args[1] as i32
    }

    #[inline(always)]
    fn sig(args: &[usize]) -> c_int {
        args[2] as c_int
    }

    #[inline(always)]
    fn uinfo(args: &[usize]) -> usize {
        args[3]
    }
}

impl Syscall for SysRtTgsigqueueinfoHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let tgid = Self::tgid(args);
        let tid = Self::tid(args);
        let sig = Self::sig(args);
        // Copy/layout validation precedes IDs and si_code checks in Linux.
        let user_info =
            unsafe { copy_siginfo_from_user(VirtAddr::new(Self::uinfo(args)), Some(sig))? };

        if tgid <= 0 || tid <= 0 {
            return Err(SystemError::EINVAL);
        }

        if (user_info.si_code >= 0 || user_info.si_code == SigCode::Tkill.as_i32())
            && ProcessManager::current_pcb().task_pid_vnr().data() != tid as usize
        {
            return Err(SystemError::EPERM);
        }
        do_send_specific(tgid, tid, sig, &mut SigInfo::from_posix(&user_info))
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("tgid", Self::tgid(args).to_string()),
            FormattedSyscallParam::new("tid", Self::tid(args).to_string()),
            FormattedSyscallParam::new("sig", Self::sig(args).to_string()),
            FormattedSyscallParam::new("uinfo", format!("{:#x}", Self::uinfo(args))),
        ]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_RT_TGSIGQUEUEINFO, SysRtTgsigqueueinfoHandle);
