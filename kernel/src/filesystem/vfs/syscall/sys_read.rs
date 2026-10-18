use system_error::SystemError;

use super::read_buffer::{ReadScratch, MAX_CHUNK};

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_READ;
use crate::filesystem::fsnotify::FsEvent;
use crate::filesystem::vfs::file::{File, FileFlags};
use crate::filesystem::vfs::{FileType, UserReadMode, MAX_RW_COUNT};
use crate::mm::{access_ok, VirtAddr};
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferWriter;
use crate::syscall::user_buffer::UserBuffer;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// System call handler for the `read` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for reading data from a file descriptor.
pub struct SysReadHandle;

impl Syscall for SysReadHandle {
    /// Returns the number of arguments expected by the `read` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the `read` system call
    ///
    /// Reads data from the specified file descriptor into a user buffer.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: File descriptor (i32)
    ///   - args[1]: Pointer to user buffer (*mut u8)
    ///   - args[2]: Length of data to read (usize)
    /// * `from_user` - Indicates if the call originates from user space
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes successfully read
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let buf_vaddr = Self::buf(args);
        let len = Self::len(args);

        if frame.is_from_user() {
            read_into_user_buffer(fd, buf_vaddr, len)
        } else {
            let file = get_read_file(fd)?;
            if len == 0 {
                let mut empty = [];
                return do_read_file(file.as_ref(), &mut empty);
            }
            // 内核态：直接借用内核缓冲区
            let mut user_buffer_writer =
                UserBufferWriter::new(buf_vaddr, len, frame.is_from_user())?;
            let user_buf = user_buffer_writer.buffer(0)?;
            do_read_file(file.as_ref(), user_buf)
        }
    }

    /// Formats the syscall parameters for display/debug purposes
    ///
    /// # Arguments
    /// * `args` - The raw syscall arguments
    ///
    /// # Returns
    /// Vector of formatted parameters with descriptive names
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("buf", format!("{:#x}", Self::buf(args) as usize)),
            FormattedSyscallParam::new("len", Self::len(args).to_string()),
        ]
    }
}

impl SysReadHandle {
    /// Extracts the file descriptor from syscall arguments
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    /// Extracts the buffer pointer from syscall arguments
    fn buf(args: &[usize]) -> *mut u8 {
        args[1] as *mut u8
    }

    /// Extracts the buffer length from syscall arguments
    fn len(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_READ, SysReadHandle);

pub(super) fn get_read_file(fd: i32) -> Result<Arc<File>, SystemError> {
    let binding = ProcessManager::current_pcb().fd_table();
    let fd_table_guard = binding.read();

    let file = fd_table_guard
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;

    // drop guard 以避免无法调度的问题
    drop(fd_table_guard);

    Ok(file)
}

fn do_read_file(file: &File, buf: &mut [u8]) -> Result<usize, SystemError> {
    if file.flags().contains(FileFlags::O_PATH) {
        return Err(SystemError::EBADF);
    }

    file.read(buf.len(), buf)
}

/// Read into a userspace buffer safely (exception-table protected) and in chunks.
///
/// Linux semantics: if a fault happens after some bytes are copied, return the number
/// of bytes copied instead of -EFAULT.
fn read_into_user_buffer(fd: i32, user_ptr: *mut u8, len: usize) -> Result<usize, SystemError> {
    let file = get_read_file(fd)?;

    let _position = file.lock_position();
    file.readable()?;
    // Linux validates the original numerical range, not its VMA mappings, and
    // only then clamps the request. EOF need not touch a valid user address.
    access_ok(VirtAddr::new(user_ptr as usize), len).map_err(|_| SystemError::EFAULT)?;
    file.verify_read_range(len)?;
    let len = len.min(MAX_RW_COUNT);
    let mut output = unsafe { UserBuffer::new(VirtAddr::new(user_ptr as usize), len) };

    let intent = file.read_io_intent();
    if file.user_read_mode_with_io_intent(intent) != UserReadMode::Buffered {
        return file
            .read_user_at_current_position_with_intent(len, &mut output, true, intent)?
            .ok_or(SystemError::EIO);
    }
    if len == 0 {
        return read_buffered_into_user(file.as_ref(), &mut output, None, intent);
    }
    if file.file_type() == FileType::Socket {
        return read_socket_into_user_buffer(file.as_ref(), &mut output);
    }

    read_buffered_into_user(file.as_ref(), &mut output, None, intent)
}

/// Shared bounded scalar transfer. An explicit cursor never updates the OFD.
pub(super) fn read_buffered_into_user(
    file: &File,
    output: &mut UserBuffer<'_>,
    position: Option<usize>,
    intent: crate::filesystem::vfs::ReadIoIntent,
) -> Result<usize, SystemError> {
    let len = output.len();
    if len == 0 {
        // Scalar reads still invoke the backing operation: generic directory
        // reads fail even for count zero. Vector empty completion stays separate.
        return file.read_empty_syscall();
    }
    let mut scratch = ReadScratch::default();
    let mut total = 0usize;

    while total < len {
        let remain = len - total;
        let chunk_len = core::cmp::min(MAX_CHUNK, remain);

        let kbuf = match scratch.prepare(chunk_len) {
            Ok(buffer) => buffer,
            Err(_) if total != 0 => break,
            Err(error) => return Err(error),
        };
        let read = match position {
            Some(offset) => {
                file.pread_into_user_syscall_chunk(offset + total, kbuf, output, total, intent)
            }
            None => file.read_syscall_chunk(kbuf, output, total, intent),
        };
        let n = match read {
            Ok(n) => n,
            Err(_) if total != 0 => break,
            Err(err) => return Err(err),
        };
        if n == 0 {
            break;
        }

        total += n;

        if n < chunk_len {
            break;
        }
    }

    if total != 0 {
        file.notify_io_event(FsEvent::ACCESS);
    }

    Ok(total)
}

fn read_socket_into_user_buffer(
    file: &File,
    output: &mut UserBuffer<'_>,
) -> Result<usize, SystemError> {
    let inode = file.inode();
    let socket = inode.as_socket().ok_or(SystemError::ENOTSOCK)?;
    let read_len = socket.read_to_user_buffer(output)?;
    if read_len != 0 {
        file.notify_io_event(FsEvent::ACCESS);
    }
    Ok(read_len)
}

#[cfg(feature = "mm_selftest")]
mod selftest;
#[cfg(feature = "mm_selftest")]
pub(crate) fn count_selftest() -> bool {
    selftest::run()
}
