use system_error::SystemError;

use crate::{arch::interrupt::TrapFrame, filesystem::vfs::{syscall::faccessat2::do_faccessat2},syscall::table::{FormattedSyscallParam, Syscall}};
use alloc::vec::Vec;
use crate::arch::syscall::nr::SYS_FACCESSAT2;

pub struct SysFaccessat2Handle;

impl Syscall for SysFaccessat2Handle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let dirfd = Self::dirfd(args);
        let pathname = Self::pathname(args);
        let mode = Self::mode(args);
        let flags = Self::flags(args);

        return do_faccessat2(
            dirfd,
            pathname,
            mode,
            flags,
        );
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dirfd", format!("{:#x}", Self::dirfd(args))),
            FormattedSyscallParam::new("pathname", format!("{:#x}", Self::pathname(args) as usize)),
            FormattedSyscallParam::new("mode", format!("{:#x}", Self::mode(args))),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

impl SysFaccessat2Handle {
    fn dirfd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn pathname(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }

    fn mode(args: &[usize]) -> u32 {
        args[2] as u32
    }

    fn flags(args: &[usize]) -> u32 {
        args[3] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_FACCESSAT2, SysFaccessat2Handle);