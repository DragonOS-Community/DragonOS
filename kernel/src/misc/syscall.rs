use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_REBOOT},
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::{string::ToString, vec::Vec};
use system_error::SystemError;

use super::reboot::do_sys_reboot;

pub struct SysRebootHandle;

impl SysRebootHandle {
    #[inline(always)]
    fn magic1(args: &[usize]) -> u32 {
        args[0] as u32
    }

    #[inline(always)]
    fn magic2(args: &[usize]) -> u32 {
        args[1] as u32
    }

    #[inline(always)]
    fn cmd(args: &[usize]) -> u32 {
        args[2] as u32
    }

    #[inline(always)]
    fn arg(args: &[usize]) -> usize {
        args[3]
    }
}

impl Syscall for SysRebootHandle {
    fn num_args(&self) -> usize {
        4
    }
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let magic1 = Self::magic1(args);
        let magic2 = Self::magic2(args);
        let cmd = Self::cmd(args);
        let arg = Self::arg(args);
        do_sys_reboot(magic1, magic2, cmd, arg).map(|_| 0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("magic1", Self::magic1(args).to_string()),
            FormattedSyscallParam::new("magic2", Self::magic2(args).to_string()),
            FormattedSyscallParam::new("cmd", Self::cmd(args).to_string()),
            FormattedSyscallParam::new("arg", Self::arg(args).to_string()),
        ]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_REBOOT, SysRebootHandle);
