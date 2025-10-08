use system_error::SystemError;

use crate::arch::syscall::nr::SYS_READLINK;
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::{fcntl::AtFlags, syscall::readlink_at::do_readlink_at},
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;

pub struct SysReadlinkHandle;

impl Syscall for SysReadlinkHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path = Self::path(args);
        let user_buf = Self::user_buf(args);
        let buf_size = Self::buf_size(args);

        return do_readlink_at(AtFlags::AT_FDCWD.bits(), path, user_buf, buf_size);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("buf", format!("{:#x}", Self::user_buf(args) as usize)),
            FormattedSyscallParam::new("buf_size", format!("{:#x}", Self::buf_size(args))),
        ]
    }
}

impl SysReadlinkHandle {
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    fn user_buf(args: &[usize]) -> *mut u8 {
        args[1] as *mut u8
    }
    fn buf_size(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_READLINK, SysReadlinkHandle);
