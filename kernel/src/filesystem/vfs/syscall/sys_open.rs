//! System call handler for opening files.

use system_error::SystemError;

use crate::arch::syscall::nr::SYS_OPEN;
use crate::filesystem::vfs::fcntl::AtFlags;
use crate::filesystem::vfs::open::do_sys_open;
use crate::filesystem::vfs::{FileMode, ModeType, MAX_PATHLEN};
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::check_and_clone_cstr;

use alloc::string::ToString;
use alloc::vec::Vec;

/// Handler for the `open` system call.
pub struct SysOpenHandle;

impl Syscall for SysOpenHandle {
    /// Returns the number of arguments this syscall takes (3).
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the open syscall by extracting arguments and calling `do_open`.
    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let path = Self::path(args);
        let flags = Self::flags(args);
        let mode = Self::mode(args);

        do_open(path, flags, mode, true)
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("flags", Self::flags(args).to_string()),
            FormattedSyscallParam::new("mode", Self::mode(args).to_string()),
        ]
    }
}

impl SysOpenHandle {
    /// Extracts the path argument from syscall parameters.
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    /// Extracts the flags argument from syscall parameters.
    fn flags(args: &[usize]) -> u32 {
        args[1] as u32
    }

    /// Extracts the mode argument from syscall parameters.
    fn mode(args: &[usize]) -> u32 {
        args[2] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_OPEN, SysOpenHandle);

/// Performs the actual file opening operation.
///
/// # Arguments
/// * `path` - Pointer to the path string
/// * `o_flags` - File opening flags
/// * `mode` - File mode/permissions
/// * `follow_symlink` - Whether to follow symbolic links
///
/// # Returns
/// File descriptor on success, or error code on failure.
pub(super) fn do_open(
    path: *const u8,
    o_flags: u32,
    mode: u32,
    follow_symlink: bool,
) -> Result<usize, SystemError> {
    let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;

    let open_flags: FileMode = FileMode::from_bits(o_flags).ok_or(SystemError::EINVAL)?;
    let mode = ModeType::from_bits(mode).ok_or(SystemError::EINVAL)?;
    return do_sys_open(
        AtFlags::AT_FDCWD.bits(),
        &path,
        open_flags,
        mode,
        follow_symlink,
    );
}
