//! System call handler for changing current working directory using file descriptor.

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_FCHDIR;
use crate::filesystem::vfs::permission::check_chdir_permission;
use crate::filesystem::vfs::FileType;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `fchdir` syscall
///
/// Changes the current working directory to the directory referenced by the file descriptor.
pub struct SysFchdirHandle;

impl Syscall for SysFchdirHandle {
    /// Returns the number of arguments expected by the `fchdir` syscall
    fn num_args(&self) -> usize {
        1
    }

    /// Changes the current working directory using a file descriptor
    ///
    /// ## Arguments
    /// - `fd`: File descriptor referring to a directory
    ///
    /// ## Returns
    /// - `Ok(0)`: Success
    /// - `Err(SystemError)`: Various error conditions (EBADF, ENOTDIR, etc.)
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);

        let pcb = ProcessManager::current_pcb();
        let file = pcb
            .fd_table()
            .read()
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;
        let inode = file.inode();
        let metadata = inode.metadata()?;

        let cred = pcb.cred();
        check_chdir_permission(&metadata, &cred)?;

        if metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        let path = inode.absolute_path()?;
        pcb.basic_mut().set_cwd(path);
        pcb.fs_struct_mut().set_pwd(inode);
        return Ok(0);
    }

    /// Formats the syscall parameters for display/debug purposes
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new("fd", Self::fd(args).to_string())]
    }
}

impl SysFchdirHandle {
    /// Extracts the file descriptor argument from syscall parameters
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }
}

syscall_table_macros::declare_syscall!(SYS_FCHDIR, SysFchdirHandle);
