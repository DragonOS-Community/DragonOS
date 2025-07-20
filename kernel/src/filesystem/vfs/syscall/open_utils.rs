use system_error::SystemError;

use crate::{
    define_event_trace,
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
    trace_sys_enter_openat(AtFlags::AT_FDCWD.bits(), path, o_flags, mode);
    let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;

    let show = crate::process::ProcessManager::current_pid().data() >= 8;
    if show {
        log::debug!(
            "do_open: path: {}, o_flags: {:?}, mode: {:?}, follow_symlink: {}",
            path,
            FileMode::from_bits(o_flags),
            ModeType::from_bits(mode),
            follow_symlink
        );
    }

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

define_event_trace!(
    sys_enter_openat,
    TP_system(syscalls),
    TP_PROTO(dfd: i32, path:*const u8, o_flags: u32, mode: u32),
    TP_STRUCT__entry{
        dfd: i32,
        path: u64,
        o_flags: u32,
        mode: u32,
    },
    TP_fast_assign{
        dfd: dfd,
        path: path as u64,
        o_flags: o_flags,
        mode: mode,
    },
    TP_ident(__entry),
    TP_printk({
        format!(
            "dfd: {}, path: {:#x}, o_flags: {:?}, mode: {:?}",
            __entry.dfd,
            __entry.path,
            __entry.o_flags,
            __entry.mode
        )
    })
);
