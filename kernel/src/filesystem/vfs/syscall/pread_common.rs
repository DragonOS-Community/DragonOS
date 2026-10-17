//! Positional scalar read admission. Data transfer uses the ordinary read core.
use crate::{
    filesystem::{
        fsnotify::FsEvent,
        vfs::{file::File, UserReadMode, MAX_RW_COUNT},
    },
    mm::{access_ok, VirtAddr},
    syscall::user_buffer::UserBuffer,
};
use system_error::SystemError;

pub(super) fn do_pread_at(
    file: &File,
    offset: usize,
    user_ptr: usize,
    len: usize,
    from_user: bool,
) -> Result<usize, SystemError> {
    file.verify_pread_access()?;
    file.readable()?;
    if from_user {
        access_ok(VirtAddr::new(user_ptr), len).map_err(|_| SystemError::EFAULT)?;
    }
    file.verify_pread_range(offset, len)?;
    let len = len.min(MAX_RW_COUNT);
    if !from_user {
        // The internal caller supplies a kernel-owned range, never a user slice.
        if len != 0 && user_ptr == 0 {
            return Err(SystemError::EFAULT);
        }
        let mut empty = [];
        let buffer = if len == 0 {
            &mut empty[..]
        } else {
            unsafe { core::slice::from_raw_parts_mut(user_ptr as *mut u8, len) }
        };
        let read = file.pread_syscall_chunk(offset, len, buffer)?;
        if read != 0 {
            file.notify_io_event(FsEvent::ACCESS);
        }
        return Ok(read);
    }
    // Numerical range validation does not probe VMAs. EOF need not access it.
    let mut output = unsafe { UserBuffer::new(VirtAddr::new(user_ptr), len) };
    if file.user_read_mode() != UserReadMode::Buffered {
        let read = file
            .pread_user_syscall_chunk(offset, len, &mut output)?
            .ok_or(SystemError::EIO)?;
        if read != 0 {
            file.notify_io_event(FsEvent::ACCESS);
        }
        return Ok(read);
    }
    super::sys_read::read_buffered_into_user(file, &mut output, Some(offset))
}
