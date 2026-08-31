use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_READV;
use crate::arch::MMArch;
use crate::filesystem::fsnotify::FsEvent;
use crate::filesystem::vfs::file::File;
use crate::filesystem::vfs::iov::IoVec;
use crate::filesystem::vfs::iov::IoVecs;
use crate::filesystem::vfs::FileType;
use crate::mm::MemoryManagementArch;
use crate::mm::VirtAddr;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::{copy_to_user_protected, user_accessible_len};
use crate::syscall::user_buffer::UserBuffer;
use alloc::string::ToString;
use alloc::vec::Vec;

use super::sys_read::get_read_file;

/// System call handler for `readv` operation
///
/// The `readv` system call reads data into multiple buffers from a file descriptor.
/// It is equivalent to multiple `read` calls but is more efficient.
pub struct SysReadVHandle;

impl Syscall for SysReadVHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let iov = Self::iov(args);
        let count = Self::count(args);

        let file = get_read_file(fd)?;
        file.readable()?;

        // Linux accepts a zero-segment readv, returns zero and publishes the
        // syscall-level ACCESS event after validating the descriptor.
        if count == 0 {
            return finish_readv(file.as_ref(), 0);
        }

        // IoVecs 会进行用户态检验(包含 len==0 的 iov_base 校验)。
        let iovecs = unsafe { IoVecs::from_user(iov, count, true) }?;

        read_iovecs(file.as_ref(), &iovecs)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("iov", format!("{:#x}", Self::iov(args) as usize)),
            FormattedSyscallParam::new("count", Self::count(args).to_string()),
        ]
    }
}

/// Complete a non-positional vectored read after the iovec array is imported.
/// Shared by readv and preadv2(offset = -1), as required by Linux semantics.
pub(super) fn read_iovecs(file: &File, iovecs: &IoVecs) -> Result<usize, SystemError> {
    // Linux: limit per readv() to MAX_RW_COUNT = INT_MAX & ~(PAGE_SIZE-1)
    let max_rw_count = (i32::MAX as usize) & !(MMArch::PAGE_SIZE - 1);

    // Linux falls back to one `.read` call per iovec when an inode does
    // not implement `.read_iter`. Record readers that consume data before
    // copying it to userspace must keep that ordering for readv as well.
    if file.supports_read_user() {
        return readv_user_chunks(file, iovecs, max_rw_count);
    }

    if file.file_type() == FileType::Socket {
        // Socket readv is one underlying read, but ACCESS still belongs to
        // the readv completion boundary (including a zero-byte result).
        let requested = iovecs.total_len().min(max_rw_count);
        if requested == 0 {
            return finish_readv(file, 0);
        }
        let segments = iovecs.user_buffer_segments(requested)?;
        let mut user_buffer = unsafe { UserBuffer::new_vectored(&segments, requested) };
        let inode = file.inode();
        let socket = inode.as_socket().ok_or(SystemError::ENOTSOCK)?;
        let nread = socket.read_to_user_buffer(&mut user_buffer)?;
        return finish_readv(file, nread);
    }

    let mut total_read: usize = 0;

    // Keep kernel-side buffer modest to avoid huge allocations.
    // Also used as the granularity for accessibility checks to avoid
    // traversing huge address ranges at once.
    const CHUNK: usize = 64 * 1024;

    for one in iovecs.iovs().iter() {
        // Check if we've reached MAX_RW_COUNT limit
        if total_read >= max_rw_count {
            break;
        }

        let remain = max_rw_count - total_read;
        let want = core::cmp::min(one.iov_len, remain);
        if want == 0 {
            continue;
        }

        let mut copied_this_iov = 0usize;
        while copied_this_iov < want {
            // Calculate how much to process in this iteration
            let remain_iov = want - copied_this_iov;
            let chunk_len = core::cmp::min(CHUNK, remain_iov);

            let current_base = one.iov_base as usize + copied_this_iov;

            // Check accessibility for this chunk only (not the entire iovec)
            // This avoids traversing huge address ranges at once
            let accessible = user_accessible_len(VirtAddr::new(current_base), chunk_len, true);
            if accessible == 0 {
                if total_read == 0 && copied_this_iov == 0 {
                    return Err(SystemError::EFAULT);
                }
                // Hit unmapped region, return what we've read so far
                return finish_readv(file, total_read);
            }

            // Read into kernel buffer
            let to_read = core::cmp::min(accessible, chunk_len);
            let mut kbuf = alloc::vec![0u8; to_read];
            let n = match file.read_syscall_chunk(to_read, &mut kbuf[..]) {
                Ok(n) => n,
                Err(_) if total_read != 0 => return finish_readv(file, total_read),
                Err(error) => return Err(error),
            };
            if n == 0 {
                // EOF
                return finish_readv(file, total_read);
            }

            // Copy to user space
            let dst = VirtAddr::new(current_base);
            let write_res = unsafe { copy_to_user_protected(dst, &kbuf[..n]) };
            match write_res {
                Ok(_) => {
                    copied_this_iov += n;
                    total_read = total_read.saturating_add(n);

                    // Check MAX_RW_COUNT limit after each chunk
                    if total_read >= max_rw_count {
                        return finish_readv(file, total_read);
                    }
                }
                Err(SystemError::EFAULT) => {
                    // Linux: return partial count if any bytes were copied.
                    if total_read == 0 {
                        return Err(SystemError::EFAULT);
                    }
                    return finish_readv(file, total_read);
                }
                Err(e) => return Err(e),
            }

            // Stop on short read (EOF or error in underlying file)
            if n < to_read {
                return finish_readv(file, total_read);
            }
        }
    }

    finish_readv(file, total_read)
}

fn finish_readv(
    file: &crate::filesystem::vfs::file::File,
    total_read: usize,
) -> Result<usize, SystemError> {
    // Linux do_iter_read() publishes one ACCESS event at the readv syscall
    // boundary, not once per iovec or bounded implementation chunk.
    file.notify_io_event(FsEvent::ACCESS);
    Ok(total_read)
}

fn readv_user_chunks(
    file: &crate::filesystem::vfs::file::File,
    iovecs: &IoVecs,
    max_rw_count: usize,
) -> Result<usize, SystemError> {
    let mut total_read = 0usize;

    for one in iovecs.iovs() {
        if total_read >= max_rw_count {
            break;
        }
        let len = one.iov_len.min(max_rw_count - total_read);
        if len == 0 {
            continue;
        }

        let mut user_buffer = unsafe { UserBuffer::new(VirtAddr::new(one.iov_base as usize), len) };
        let read_len = match file.read_user_syscall_chunk(len, &mut user_buffer) {
            Ok(Some(read_len)) => read_len,
            Ok(None) => {
                debug_assert!(
                    false,
                    "supports_read_user without read_user_at implementation"
                );
                return Err(SystemError::ENOSYS);
            }
            Err(_) if total_read != 0 => return finish_readv(file, total_read),
            Err(error) => return Err(error),
        };
        total_read = total_read.saturating_add(read_len);

        if read_len != len {
            break;
        }
    }

    finish_readv(file, total_read)
}

impl SysReadVHandle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn iov(args: &[usize]) -> *const IoVec {
        args[1] as *const IoVec
    }

    fn count(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_READV, SysReadVHandle);
