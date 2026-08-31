use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_READ;
use crate::filesystem::fsnotify::FsEvent;
use crate::filesystem::vfs::file::{File, FileFlags};
use crate::filesystem::vfs::FileType;
use crate::mm::VirtAddr;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::{copy_to_user_protected, user_accessible_len, UserBufferWriter};
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

    // Some record streams must own the whole userspace read boundary. In
    // particular, inotify decides whether to wait again and consumes a record
    // even when copying that record faults, matching Linux inotify_read().
    // Use the original count here so a partially mapped range faults at the
    // exact record copy instead of being silently shortened to `accessible`.
    if file.supports_read_user() {
        // Do not validate the complete `count` range here.  Linux record
        // readers first apply their count rule and then touch only the bytes
        // they actually return (timerfd, for example, always writes 8 bytes).
        // Every access through UserBuffer remains exception-table protected.
        let mut direct_buffer = unsafe { UserBuffer::new(VirtAddr::new(user_ptr as usize), len) };
        if let Some(read_len) = file.read_user(len, &mut direct_buffer)? {
            return Ok(read_len);
        }
        debug_assert!(
            false,
            "supports_read_user without read_user_at implementation"
        );
    }

    // Linux still validates the descriptor and access mode for a zero-length
    // read. Direct-user inodes above own any type-specific count==0 rule.
    if len == 0 {
        file.readable()?;
        return Ok(0);
    }

    // Ordinary buffered reads may probe the mapped prefix up front. Record
    // streams that require consume-before-copy semantics were dispatched above.
    let accessible =
        user_accessible_len(VirtAddr::new(user_ptr as usize), len, true /*write*/);
    if accessible == 0 {
        return Err(SystemError::EFAULT);
    }

    if file.file_type() == FileType::Socket {
        return read_socket_into_user_buffer(file.as_ref(), user_ptr, accessible);
    }

    // Keep the kernel-side buffer modest to avoid huge allocations/long critical sections.
    const CHUNK: usize = 64 * 1024;
    let mut total = 0usize;

    while total < accessible {
        let remain = accessible - total;
        let chunk_len = core::cmp::min(CHUNK, remain);

        let mut kbuf = alloc::vec![0u8; chunk_len];
        let n = match file.read_syscall_chunk(chunk_len, &mut kbuf[..]) {
            Ok(n) => n,
            Err(_) if total != 0 => break,
            Err(err) => return Err(err),
        };
        if n == 0 {
            break;
        }

        let dst = VirtAddr::new(user_ptr as usize + total);
        let write_res = unsafe { copy_to_user_protected(dst, &kbuf[..n]) };
        match write_res {
            Ok(_) => {
                total += n;
            }
            Err(SystemError::EFAULT) => {
                if total == 0 {
                    return Err(SystemError::EFAULT);
                }
                break;
            }
            Err(e) => return Err(e),
        }

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
    user_ptr: *mut u8,
    accessible: usize,
) -> Result<usize, SystemError> {
    let inode = file.inode();
    let socket = inode.as_socket().ok_or(SystemError::ENOTSOCK)?;

    let mut writer = UserBufferWriter::new(user_ptr, accessible, true)?;
    let mut user_buffer = writer.buffer_protected(0)?;
    let read_len = socket.read_to_user_buffer(&mut user_buffer)?;
    if read_len != 0 {
        file.notify_io_event(FsEvent::ACCESS);
    }
    Ok(read_len)
}
