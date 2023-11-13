use crate::syscall::SystemError;

use super::{fcntl::AtFlags, syscall::ModeType};

pub(super) fn do_faccessat(
    _dirfd: i32,
    _pathname: *const u8,
    mode: ModeType,
    flags: u32,
) -> Result<usize, SystemError> {
    if (mode.bits() & (!ModeType::S_IXUGO.bits())) != 0 {
        return Err(SystemError::EINVAL);
    }

    if (flags
        & (!((AtFlags::AT_EACCESS | AtFlags::AT_SYMLINK_NOFOLLOW | AtFlags::AT_EMPTY_PATH).bits()
            as u32)))
        != 0
    {
        return Err(SystemError::EINVAL);
    }

    // let follow_symlink = flags & AtFlags::AT_SYMLINK_NOFOLLOW.bits() as u32 == 0;

    // todo: 接着完善（可以借鉴linux 6.1.9的do_faccessat）
    return Ok(0);
}
