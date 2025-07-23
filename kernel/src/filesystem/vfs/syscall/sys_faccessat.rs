use system_error::SystemError;

use crate::{arch::interrupt::TrapFrame, filesystem::vfs::{syscall::faccessat2::do_faccessat2},syscall::table::{FormattedSyscallParam, Syscall}};
use alloc::vec::Vec;
use crate::arch::syscall::nr::SYS_FACCESSAT;

pub struct SysFaccessatHandle;

impl Syscall for SysFaccessatHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let dirfd = Self::dirfd(args);
        let pathname = Self::pathname(args);
        let mode = Self::mode(args);

        return do_faccessat2(
            dirfd,
            pathname,
            mode,
            0,
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

impl SysFaccessatHandle {
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

syscall_table_macros::declare_syscall!(SYS_FACCESSAT, SysFaccessatHandle);