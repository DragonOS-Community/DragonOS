use system_error::SystemError;

use crate::filesystem::vfs::{open::do_faccessat, syscall::ModeType};

pub fn do_faccessat2(
    dirfd: i32,
    pathname: *const u8,
    mode: u32,
    flags: u32,
) -> Result<usize, SystemError> {
    return do_faccessat(
        dirfd,
        pathname,
        ModeType::from_bits(mode).ok_or(SystemError::EINVAL)?,
        flags,
    );
}
