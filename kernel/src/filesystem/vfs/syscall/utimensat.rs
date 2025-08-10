use system_error::SystemError;

use crate::{
    filesystem::vfs::{MAX_PATHLEN, open::do_utimensat, syscall::UtimensFlags},
    syscall::user_access::{UserBufferReader, check_and_clone_cstr},
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
        let pathname = check_and_clone_cstr(pathname, Some(MAX_PATHLEN))?
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
        Some([times[0], times[1]])
    };
    do_utimensat(dirfd, pathname, times, flags)
}
