use alloc::vec::Vec;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_PTRACE},
    process::{ptrace, ProcessManager, RawPid},
    syscall::table::{FormattedSyscallParam, Syscall},
};
use system_error::SystemError;

const PTRACE_TRACEME: usize = 0;

pub struct SysPtrace;

impl SysPtrace {
    fn request(args: &[usize]) -> usize {
        args[0]
    }

    fn pid(args: &[usize]) -> i32 {
        args[1] as i32
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
        match Self::request(args) {
            PTRACE_TRACEME => {
                ptrace::traceme_current()?;
                Ok(0)
            }
            _ => {
                let pid = Self::pid(args);
                if pid <= 0
                    || ProcessManager::find_task_by_vpid(RawPid::new(pid as usize)).is_none()
                {
                    return Err(SystemError::ESRCH);
                }
                Err(SystemError::EIO)
            }
        }
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("request", format!("{:#x}", Self::request(args))),
            FormattedSyscallParam::new("pid", format!("{:#x}", Self::pid(args))),
            FormattedSyscallParam::new("addr", format!("{:#x}", Self::addr(args))),
            FormattedSyscallParam::new("data", format!("{:#x}", Self::data(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_PTRACE, SysPtrace);
