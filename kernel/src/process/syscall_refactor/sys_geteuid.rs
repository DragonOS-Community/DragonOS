use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETEUID;
use crate::process::geteuid::do_geteuid;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysGetEuid;

impl Syscall for SysGetEuid {
    fn num_args(&self) -> usize {
        0
    }

    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        do_geteuid()
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETEUID, SysGetEuid);
