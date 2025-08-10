use system_error::SystemError;

use crate::arch::syscall::nr::SYS_FCHMODAT;
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::{open::do_fchmodat, syscall::ModeType},
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;

pub struct SysFchmodatHandle;

impl Syscall for SysFchmodatHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let dirfd = Self::dirfd(args);
        let pathname = Self::pathname(args);
        let mode = Self::mode(args);

        return do_fchmodat(
            dirfd,
            pathname,
            ModeType::from_bits(mode).ok_or(SystemError::EINVAL)?,
        );
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dirfd", format!("{:#x}", Self::dirfd(args))),
            FormattedSyscallParam::new("pathname", format!("{:#x}", Self::pathname(args) as usize)),
            FormattedSyscallParam::new("mode", format!("{:#x}", Self::mode(args))),
        ]
    }
}

impl SysFchmodatHandle {
    fn dirfd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn pathname(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }

    fn mode(args: &[usize]) -> u32 {
        args[2] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_FCHMODAT, SysFchmodatHandle);
