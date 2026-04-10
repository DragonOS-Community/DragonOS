use system_error::SystemError;

use crate::arch::syscall::nr::SYS_FCHOWN;
use crate::filesystem::vfs::file::FileFlags;
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::open::ksys_fchown,
    process::ProcessManager,
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

        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        // Linux 语义：对 O_PATH fd 执行 fchown 应返回 EBADF
        if file.flags().contains(FileFlags::O_PATH) {
            return Err(SystemError::EBADF);
        }
        drop(fd_table_guard);

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
