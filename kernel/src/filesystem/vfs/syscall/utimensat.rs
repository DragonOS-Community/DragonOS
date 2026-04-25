use system_error::SystemError;

use crate::{
    filesystem::vfs::{open::do_utimensat, syscall::UtimensFlags, MAX_PATHLEN},
    syscall::user_access::{vfs_check_and_clone_cstr, UserBufferReader},
    time::PosixTimeSpec,
};

pub fn do_sys_utimensat(
    dirfd: i32,
    pathname: *const u8,
    times: *const PosixTimeSpec,
    flags: u32,
) -> Result<usize, SystemError> {
    let pathname = if pathname.is_null() {
        None
    } else {
        let pathname = vfs_check_and_clone_cstr(pathname, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        Some(pathname)
    };
    let flags = UtimensFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;
    let times = if times.is_null() {
        None
    } else {
        let times_reader = UserBufferReader::new(times, size_of::<PosixTimeSpec>() * 2, true)?;
        let times = times_reader.read_from_user::<PosixTimeSpec>(0)?;
        let atime = times[0];
        let mtime = times[1];

        // Validate tv_nsec: it must be in [0, 1e9) or be UTIME_NOW/UTIME_OMIT.
        const UTIME_NOW: i64 = (1i64 << 30) - 1i64;
        const UTIME_OMIT: i64 = (1i64 << 30) - 2i64;

        let valid_nsec = |nsec: i64| -> bool {
            (0..1_000_000_000).contains(&nsec) || nsec == UTIME_NOW || nsec == UTIME_OMIT
        };

        if !valid_nsec(atime.tv_nsec) || !valid_nsec(mtime.tv_nsec) {
            return Err(SystemError::EINVAL);
        }

        Some([atime, mtime])
    };
    do_utimensat(dirfd, pathname, times, flags)
}
