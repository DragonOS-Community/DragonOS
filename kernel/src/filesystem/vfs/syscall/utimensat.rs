use alloc::string::String;
use system_error::SystemError;

use crate::{
    filesystem::vfs::{
        open::{do_utimensat, UTIME_OMIT},
        syscall::UtimensFlags,
        MAX_PATHLEN,
    },
    syscall::user_access::{vfs_check_and_clone_cstr, UserBufferReader},
    time::PosixTimeSpec,
};

pub(super) fn read_timestamp_path(pathname: *const u8) -> Result<Option<String>, SystemError> {
    if pathname.is_null() {
        return Ok(None);
    }

    vfs_check_and_clone_cstr(pathname, Some(MAX_PATHLEN))?
        .into_string()
        .map(Some)
        .map_err(|_| SystemError::EINVAL)
}

pub(super) fn do_sys_utimensat(
    dirfd: i32,
    pathname: *const u8,
    times: *const PosixTimeSpec,
    flags: u32,
) -> Result<usize, SystemError> {
    let times = if times.is_null() {
        None
    } else {
        let reader = UserBufferReader::new(times, size_of::<[PosixTimeSpec; 2]>(), true)?;
        let values = reader.read_one_from_user::<[PosixTimeSpec; 2]>(0)?;
        // A double UTIME_OMIT short-circuits even pathname and flag validation.
        if values.iter().all(|time| time.tv_nsec == UTIME_OMIT) {
            return Ok(0);
        }
        Some(values)
    };
    let flags = UtimensFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;
    do_utimensat(dirfd, read_timestamp_path(pathname)?, times, flags)
}
