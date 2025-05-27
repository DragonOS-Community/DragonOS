use system_error::SystemError;

use crate::{
    filesystem::vfs::{fcntl::AtFlags, file::FileMode, open::do_sys_open, MAX_PATHLEN},
    syscall::user_access::check_and_clone_cstr,
};

use super::ModeType;

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
