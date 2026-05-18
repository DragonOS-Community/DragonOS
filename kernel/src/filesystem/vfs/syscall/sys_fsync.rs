use alloc::string::ToString;

use alloc::vec::Vec;

use crate::{
    arch::{
        interrupt::TrapFrame,
        syscall::nr::{SYS_FDATASYNC, SYS_FSYNC},
    },
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use system_error::SystemError;

fn do_fsync(fd: i32, datasync: bool) -> Result<usize, SystemError> {
    let binding = ProcessManager::current_pcb().fd_table();
    let fd_table_guard = binding.read();
    let file = fd_table_guard
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;
    drop(fd_table_guard);

    file.inode().sync_file(datasync, file.private_data.lock())?;
    Ok(0)
}

/// synchronize a file's in-core state with storage device.
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
        do_fsync(fd, false)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new("fd", args[0].to_string())]
    }
}

syscall_table_macros::declare_syscall!(SYS_FSYNC, SysFsyncHandle);

/// synchronize a file's data with storage.
///
/// See Linux sys_fdatasync: it is fsync with the datasync flag set.
pub struct SysFdatasyncHandle;

impl Syscall for SysFdatasyncHandle {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(
        &self,
        args: &[usize],
        _frame: &mut TrapFrame,
    ) -> Result<usize, system_error::SystemError> {
        let fd = args[0] as i32;
        do_fsync(fd, true)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new("fd", args[0].to_string())]
    }
}

syscall_table_macros::declare_syscall!(SYS_FDATASYNC, SysFdatasyncHandle);
