use system_error::SystemError;

use crate::arch::syscall::nr::SYS_NEWFSTATAT;
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::{stat::do_newfstatat, MAX_PATHLEN},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::check_and_clone_cstr,
    },
};
use alloc::vec::Vec;

pub struct SysNewFstatatHandle;

impl Syscall for SysNewFstatatHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let dfd = Self::dfd(args);
        let filename_ptr = Self::filename_ptr(args);
        let user_stat_buf_ptr = Self::user_stat_buf_ptr(args);
        let flags = Self::flags(args);
        Self::newfstatat(dfd, filename_ptr, user_stat_buf_ptr, flags)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dfd", format!("{:#x}", Self::dfd(args))),
            FormattedSyscallParam::new("filename", format!("{:#x}", Self::filename_ptr(args))),
            FormattedSyscallParam::new(
                "user_stat_buf_ptr",
                format!("{:#x}", Self::user_stat_buf_ptr(args)),
            ),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

impl SysNewFstatatHandle {
    fn dfd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn filename_ptr(args: &[usize]) -> usize {
        args[1]
    }

    fn user_stat_buf_ptr(args: &[usize]) -> usize {
        args[2]
    }

    fn flags(args: &[usize]) -> u32 {
        args[3] as u32
    }

    #[inline(never)]
    fn newfstatat(
        dfd: i32,
        filename_ptr: usize,
        user_stat_buf_ptr: usize,
        flags: u32,
    ) -> Result<usize, SystemError> {
        if user_stat_buf_ptr == 0 {
            return Err(SystemError::EFAULT);
        }

        let filename = check_and_clone_cstr(filename_ptr as *const u8, Some(MAX_PATHLEN))?;
        let filename_str = filename.to_str().map_err(|_| SystemError::EINVAL)?;

        do_newfstatat(dfd, filename_str, user_stat_buf_ptr, flags).map(|_| 0)
    }
}

syscall_table_macros::declare_syscall!(SYS_NEWFSTATAT, SysNewFstatatHandle);
