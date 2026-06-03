use alloc::{format, vec::Vec};

use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_IOPERM},
    syscall::table::{FormattedSyscallParam, Syscall},
};

pub struct SysIoperm;

impl SysIoperm {
    fn from(args: &[usize]) -> usize {
        args[0]
    }

    fn num(args: &[usize]) -> usize {
        args[1]
    }

    fn turn_on(args: &[usize]) -> bool {
        args[2] != 0
    }
}

impl Syscall for SysIoperm {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        crate::arch::process::io_bitmap::do_ioperm(
            Self::from(args),
            Self::num(args),
            Self::turn_on(args),
        )
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("from", format!("{:#x}", Self::from(args))),
            FormattedSyscallParam::new("num", format!("{}", Self::num(args))),
            FormattedSyscallParam::new("turn_on", format!("{}", args[2])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_IOPERM, SysIoperm);
