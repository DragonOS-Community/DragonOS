use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_FCHMODAT2},
    filesystem::vfs::open::do_fchmodat,
    syscall::table::{FormattedSyscallParam, Syscall},
};

pub struct SysFchmodat2Handle;

impl Syscall for SysFchmodat2Handle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        do_fchmodat(
            args[0] as i32,
            args[1] as *const u8,
            args[2] as u32,
            args[3] as u32,
        )
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dirfd", format!("{:#x}", args[0] as i32)),
            FormattedSyscallParam::new("pathname", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("mode", format!("{:#x}", args[2] as u32)),
            FormattedSyscallParam::new("flags", format!("{:#x}", args[3] as u32)),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_FCHMODAT2, SysFchmodat2Handle);
