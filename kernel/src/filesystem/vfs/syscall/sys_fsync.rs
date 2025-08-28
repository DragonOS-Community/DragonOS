use alloc::string::ToString;

use alloc::vec::Vec;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_FSYNC},
    syscall::table::{FormattedSyscallParam, Syscall},
};

/// synchronize a file's in-core state with storagedevice.
///
/// See https://man7.org/linux/man-pages/man2/fsync.2.html
pub struct SysFsyncHandle;

impl Syscall for SysFsyncHandle {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(
        &self,
        args: &[usize],
        _frame: &mut TrapFrame,
    ) -> Result<usize, system_error::SystemError> {
        let fd = args[0] as i32;
        let binding = crate::process::ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(system_error::SystemError::EBADF)?;
        drop(fd_table_guard);
        let inode = file.inode();
        inode.sync()?;
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new("fd", args[0].to_string())]
    }
}

syscall_table_macros::declare_syscall!(SYS_FSYNC, SysFsyncHandle);
