//! System call handler for ioctls.

use crate::arch::syscall::nr::SYS_IOCTL;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use system_error::SystemError;

use alloc::string::ToString;
use alloc::vec::Vec;

/// Handler for the `ioctl` system call.
pub struct SysIoctlHandle;

impl Syscall for SysIoctlHandle {
    /// Returns the number of arguments this syscall takes (3).
    fn num_args(&self) -> usize {
        3
    }

    /// Sends a command to the device corresponding to the file descriptor.
    ///
    /// # Arguments
    ///
    /// * `fd` - File descriptor number
    /// * `cmd` - Device-dependent request code
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - On success, returns 0
    /// * `Err(SystemError)` - On failure, returns a POSIX error code
    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let cmd = Self::cmd(args);
        let data = Self::data(args);

        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        let file = fd_table_guard
            .get_file_by_fd(fd as i32)
            .ok_or(SystemError::EBADF)?;

        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);
        let r = file.inode().ioctl(cmd, data, &file.private_data.lock());
        return r;
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("cmd", format!("{:#x}", Self::cmd(args))),
            FormattedSyscallParam::new("data", format!("{:#x}", Self::data(args))),
        ]
    }
}

impl SysIoctlHandle {
    /// Extracts the file descriptor argument from syscall parameters.
    fn fd(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the command argument from syscall parameters.
    fn cmd(args: &[usize]) -> u32 {
        args[1] as u32
    }

    /// Extracts the data argument from syscall parameters.
    fn data(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_IOCTL, SysIoctlHandle);
