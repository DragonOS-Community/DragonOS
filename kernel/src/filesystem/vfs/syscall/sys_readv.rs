use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_READV;
use crate::filesystem::fsnotify::FsEvent;
use crate::filesystem::vfs::file::File;
use crate::filesystem::vfs::iov::IoVec;
use crate::filesystem::vfs::iov::IoVecs;
use crate::filesystem::vfs::{FileType, UserReadMode, MAX_RW_COUNT};
use crate::mm::VirtAddr;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_buffer::UserBuffer;
use alloc::string::ToString;
use alloc::vec::Vec;

use super::{
    read_buffer::{ReadScratch, MAX_CHUNK},
    sys_read::get_read_file,
};

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

        readv_from_user(fd, iov, count)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("iov", format!("{:#x}", Self::iov(args) as usize)),
            FormattedSyscallParam::new("count", Self::count(args).to_string()),
        ]
    }
}

/// Common fd admission and vector import for readv and preadv2(offset=-1).
pub(super) fn readv_from_user(
    fd: i32,
    iov: *const IoVec,
    count: usize,
) -> Result<usize, SystemError> {
    let file = get_read_file(fd)?;
    // Linux fdget excludes O_PATH before importing the vector; the other
    // mode checks belong after import_iovec in Linux 6.6.
    if file
        .flags()
        .contains(crate::filesystem::vfs::file::FileFlags::O_PATH)
    {
        return Err(SystemError::EBADF);
    }
    let iovecs = unsafe { IoVecs::from_user(iov, count, true) }?;

    read_iovecs(file.as_ref(), &iovecs)
}

/// Complete a non-positional vectored read after the iovec array is imported.
/// Shared by readv and preadv2(offset = -1), as required by Linux semantics.
pub(super) fn read_iovecs(file: &File, iovecs: &IoVecs) -> Result<usize, SystemError> {
    file.readable()?;
    let max_rw_count = MAX_RW_COUNT;
    let requested = iovecs.total_len().min(max_rw_count);
    if requested == 0 {
        return finish_readv(file, 0);
    }
    file.verify_read_range(requested)?;

    // Linux falls back to one `.read` call per iovec when an inode does
    // not implement `.read_iter`. Record readers that consume data before
    // copying it to userspace must keep that ordering for readv as well.
    if file.user_read_mode() == UserReadMode::Scalar {
        return readv_user_chunks(file, iovecs, max_rw_count);
    }

    if file.user_read_mode() == UserReadMode::Iterator || file.file_type() == FileType::Socket {
        // Socket readv is one underlying read, but ACCESS still belongs to
        // the readv completion boundary (including a zero-byte result).
        let segments = iovecs.user_buffer_segments(requested)?;
        let mut user_buffer = unsafe { UserBuffer::new_vectored(&segments, requested) };
        let nread = if file.user_read_mode() == UserReadMode::Iterator {
            file.read_user_syscall_chunk(requested, &mut user_buffer)?
                .ok_or(SystemError::EIO)?
        } else {
            let inode = file.inode();
            let socket = inode.as_socket().ok_or(SystemError::ENOTSOCK)?;
            socket.read_to_user_buffer(&mut user_buffer)?
        };
        return finish_readv(file, nread);
    }

    let mut total_read: usize = 0;

    // Reuse one bounded CPU owner across implementation chunks and iovecs.
    let mut scratch = ReadScratch::default();

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
            let chunk_len = core::cmp::min(MAX_CHUNK, remain_iov);

            let current_base = one.iov_base as usize + copied_this_iov;

            let kbuf = match scratch.prepare(chunk_len) {
                Ok(buffer) => buffer,
                Err(_) if total_read != 0 => return finish_readv(file, total_read),
                Err(error) => return Err(error),
            };
            let mut output = unsafe { UserBuffer::new(VirtAddr::new(current_base), chunk_len) };
            let n = match file.read_syscall_chunk(kbuf, &mut output, 0) {
                Ok(n) => n,
                Err(_) if total_read != 0 => return finish_readv(file, total_read),
                Err(error) => return Err(error),
            };
            if n == 0 {
                // EOF
                return finish_readv(file, total_read);
            }

            copied_this_iov += n;
            total_read += n;

            // Stop on short read (EOF or error in underlying file)
            if n < chunk_len {
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
                return Err(SystemError::EIO);
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
