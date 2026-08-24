use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    filesystem::{fsnotify::FsEvent, vfs::file::File},
    mm::VirtAddr,
    syscall::user_access::{copy_from_user_protected, copy_to_user_protected, user_accessible_len},
};

pub(super) enum PreadPwriteDir {
    Read,
    Write,
}

/// Common implementation for pread64/pwrite64 with Linux-compatible "partial bad buffer" semantics.
///
/// Key rules:
/// - If `len == 0`, must not touch user memory.
/// - If the user buffer is partially inaccessible, return the number of bytes successfully
///   transferred; only return `EFAULT` if **0 bytes** were transferred.
///
/// `from_user` controls whether the pointer should be treated as a userspace pointer.
pub(super) fn do_pread_pwrite_at(
    file: &File,
    offset: usize,
    user_ptr: usize,
    len: usize,
    from_user: bool,
    dir: PreadPwriteDir,
) -> Result<usize, SystemError> {
    if len == 0 {
        return match dir {
            PreadPwriteDir::Read => {
                let mut empty: [u8; 0] = [];
                file.pread_syscall_chunk(offset, 0, &mut empty)
            }
            PreadPwriteDir::Write => file.pwrite_syscall_chunk(offset, 0, &[]),
        };
    }

    if from_user && user_ptr == 0 {
        return Err(SystemError::EFAULT);
    }

    const CHUNK: usize = 64 * 1024;
    let mut total: usize = 0;
    let mut cur_off: usize = offset;

    while total < len {
        let remain = len - total;
        let want = core::cmp::min(CHUNK, remain);

        let user_addr = VirtAddr::new(user_ptr.checked_add(total).ok_or(SystemError::EFAULT)?);
        let check_write = matches!(dir, PreadPwriteDir::Read);

        let accessible = if from_user {
            user_accessible_len(user_addr, want, check_write)
        } else {
            want
        };

        if accessible == 0 {
            if total == 0 {
                return Err(SystemError::EFAULT);
            }
            break;
        }

        let mut kbuf = Vec::new();
        if kbuf.try_reserve_exact(accessible).is_err() {
            if total == 0 {
                return Err(SystemError::ENOMEM);
            }
            break;
        }
        kbuf.resize(accessible, 0);

        match dir {
            PreadPwriteDir::Read => {
                let n = match file.pread_syscall_chunk(cur_off, accessible, &mut kbuf) {
                    Ok(n) => n,
                    Err(error) if total == 0 => return Err(error),
                    Err(_) => break,
                };
                if n == 0 {
                    break;
                }

                if from_user {
                    match unsafe { copy_to_user_protected(user_addr, &kbuf[..n]) } {
                        Ok(_) => {
                            total = total.saturating_add(n);
                            cur_off = cur_off.checked_add(n).ok_or(SystemError::EINVAL)?;
                        }
                        Err(SystemError::EFAULT) => {
                            if total == 0 {
                                return Err(SystemError::EFAULT);
                            }
                            break;
                        }
                        Err(e) => return Err(e),
                    }
                } else {
                    let dst =
                        unsafe { core::slice::from_raw_parts_mut(user_addr.data() as *mut u8, n) };
                    dst.copy_from_slice(&kbuf[..n]);
                    total = total.saturating_add(n);
                    cur_off = cur_off.checked_add(n).ok_or(SystemError::EINVAL)?;
                }

                // Stop on short file read (EOF) or when we intentionally clipped to accessible area.
                if n < accessible || accessible < want {
                    break;
                }
            }
            PreadPwriteDir::Write => {
                if from_user {
                    match unsafe { copy_from_user_protected(&mut kbuf, user_addr) } {
                        Ok(_) => {}
                        Err(SystemError::EFAULT) => {
                            if total == 0 {
                                return Err(SystemError::EFAULT);
                            }
                            break;
                        }
                        Err(e) => return Err(e),
                    }
                } else {
                    let src = unsafe {
                        core::slice::from_raw_parts(user_addr.data() as *const u8, accessible)
                    };
                    kbuf.copy_from_slice(src);
                }

                let n = match file.pwrite_syscall_chunk(cur_off, accessible, &kbuf) {
                    Ok(n) => n,
                    Err(error) if total == 0 => return Err(error),
                    Err(_) => break,
                };
                total = total.saturating_add(n);
                cur_off = cur_off.checked_add(n).ok_or(SystemError::EINVAL)?;

                // Stop on short write or when we intentionally clipped to accessible area.
                if n < accessible || accessible < want {
                    break;
                }
            }
        }
    }

    if total > 0 {
        file.notify_io_event(match dir {
            PreadPwriteDir::Read => FsEvent::ACCESS,
            PreadPwriteDir::Write => FsEvent::MODIFY,
        });
    }
    Ok(total)
}
