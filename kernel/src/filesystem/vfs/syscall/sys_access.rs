use system_error::SystemError;

use crate::arch::syscall::nr::SYS_ACCESS;
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::{fcntl::AtFlags, open::do_faccessat, InodeMode},
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;

pub struct SysAccessHandle;

impl Syscall for SysAccessHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pathname = Self::pathname(args);
        let mode = Self::mode(args);
        return do_faccessat(
            AtFlags::AT_FDCWD.bits(),
            pathname,
            InodeMode::from_bits(mode).ok_or(SystemError::EINVAL)?,
            0,
        );
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pathname", format!("{:#x}", Self::pathname(args) as usize)),
            FormattedSyscallParam::new("mode", format!("{:#x}", Self::mode(args))),
        ]
    }
}

impl SysAccessHandle {
    fn pathname(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    fn mode(args: &[usize]) -> u32 {
        args[1] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_ACCESS, SysAccessHandle);
