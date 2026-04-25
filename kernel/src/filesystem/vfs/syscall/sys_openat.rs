//! System call handler for opening files relative to a directory.

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_OPENAT;
use crate::filesystem::vfs::file::FileFlags;
use crate::filesystem::vfs::open::do_sys_open;
use crate::filesystem::vfs::InodeMode;
use crate::filesystem::vfs::MAX_PATHLEN;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::vfs_check_and_clone_cstr;
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `openat` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for opening files
/// relative to a directory file descriptor.
pub struct SysOpenatHandle;

impl Syscall for SysOpenatHandle {
    /// Returns the number of arguments expected by the `openat` syscall
    fn num_args(&self) -> usize {
        4
    }

    /// Handles the `openat` system call
    ///
    /// Opens a file relative to a directory file descriptor.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: Directory file descriptor (i32)
    ///   - args[1]: Pointer to path string (*const u8)
    ///   - args[2]: Open flags (u32)
    ///   - args[3]: File mode/permissions (u32)
    /// * `frame` - Trap frame containing context information
    ///
    /// # Returns
    /// * `Ok(usize)` - File descriptor of the opened file
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let dirfd = Self::dirfd(args);
        let path_ptr = Self::path(args);
        let o_flags = Self::o_flags(args);
        let mode = Self::mode(args);
        let path = vfs_check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let open_flags = FileFlags::from_bits(o_flags).ok_or(SystemError::EINVAL)?;
        let mode_type = InodeMode::from_bits(mode).ok_or(SystemError::EINVAL)?;
        return do_sys_open(dirfd, &path, open_flags, mode_type);
    }

    /// Formats the syscall parameters for display/debug purposes
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dirfd", Self::dirfd(args).to_string()),
            FormattedSyscallParam::new("pathname", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::o_flags(args))),
            FormattedSyscallParam::new("mode", format!("{:#o}", Self::mode(args))),
        ]
    }
}

impl SysOpenatHandle {
    /// Extracts the directory file descriptor from syscall arguments
    fn dirfd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    /// Extracts the path pointer from syscall arguments
    fn path(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }

    /// Extracts the open flags from syscall arguments
    fn o_flags(args: &[usize]) -> u32 {
        args[2] as u32
    }

    /// Extracts the file mode/permissions from syscall arguments
    fn mode(args: &[usize]) -> u32 {
        args[3] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_OPENAT, SysOpenatHandle);
