use crate::arch::ipc::signal::Signal;
use crate::ipc::signal_types::SigCode;
use crate::ipc::signal_types::{SigInfo, SigType};
use alloc::string::ToString;
use alloc::vec::Vec;
use core::ffi::c_int;

use crate::arch::interrupt::TrapFrame;
use crate::process::RawPid;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::{arch::syscall::nr::SYS_PIDFD_SEND_SIGNAL, process::ProcessManager};
use system_error::SystemError;

pub struct SysPidfdSendSignalHandle;

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
    fn siginfo(args: &[usize]) -> *mut i32 {
        args[2] as *mut i32
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
        let _sig_info = Self::siginfo(args);
        let _flags = Self::flags(args);

        // TODO: 完整的支持此系统调用
        let mut pid = 0;
        let file = ProcessManager::current_pcb()
            .fd_table()
            .read()
            .get_file_by_fd(pidfd)
            .ok_or(SystemError::EBADF)?;
        if file.private_data.lock().is_pid() {
            pid = file.private_data.lock().get_pid();
        }

        let sig = Signal::from(sig_c_int);
        if sig == Signal::INVALID {
            // log::warn!("Pidfd_Send_Signal: Send empty sig(0)");
            // 这里的信号是 0, 是空信号值, 其他的信号处理是怎样的不清楚, 但是这里应该直接返回成功, 因为 0 是空信号
            return Ok(0);
        }

        // 应该从参数获取
        let mut info = SigInfo::new(
            sig,
            0,
            SigCode::User,
            SigType::Kill(RawPid::new(pid as usize)),
        );

        let ret = sig
            .send_signal_info(Some(&mut info), RawPid::new(pid as usize))
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
