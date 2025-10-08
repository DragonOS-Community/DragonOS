use system_error::SystemError;

use crate::arch::syscall::nr::SYS_FCHOWN;
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::open::ksys_fchown,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;

pub struct SysFchownHandle;

impl Syscall for SysFchownHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let uid = Self::uid(args);
        let gid = Self::gid(args);
        ksys_fchown(fd, uid, gid)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", format!("{:#x}", Self::fd(args))),
            FormattedSyscallParam::new("uid", format!("{:#x}", Self::uid(args))),
            FormattedSyscallParam::new("gid", format!("{:#x}", Self::gid(args))),
        ]
    }
}

impl SysFchownHandle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }
    fn uid(args: &[usize]) -> usize {
        args[1]
    }

    fn gid(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_FCHOWN, SysFchownHandle);
