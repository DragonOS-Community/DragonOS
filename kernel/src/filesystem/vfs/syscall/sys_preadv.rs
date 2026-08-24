use alloc::string::ToString;
use alloc::vec::Vec;

use system_error::SystemError;

use crate::arch::syscall::nr::SYS_PREADV;
use crate::filesystem::fsnotify::FsEvent;
use crate::filesystem::vfs::file::File;
use crate::filesystem::vfs::iov::{IoVec, IoVecs};
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};

pub struct SysPreadVHandle;

impl Syscall for SysPreadVHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(
        &self,
        args: &[usize],
        _frame: &mut crate::arch::interrupt::TrapFrame,
    ) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let iov = Self::iov(args);
        let iov_count = Self::iov_count(args);
        let offset = Self::offset(args);
        if offset < 0 {
            return Err(SystemError::EINVAL);
        }

        // Construct IoVecs from user pointer.
        // For preadv, we are writing to user buffers, so we need to verify they are writable.
        // IoVecs::from_user internally uses UserBufferWriter::new which verifies the area.
        let iovecs = unsafe { IoVecs::from_user(iov, iov_count, true) }?;

        do_preadv(fd, &iovecs, offset)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd:", Self::fd(args).to_string()),
            FormattedSyscallParam::new("iov:", format!("{:#x}", Self::iov(args) as usize)),
            FormattedSyscallParam::new("iov_count:", Self::iov_count(args).to_string()),
            FormattedSyscallParam::new("offset:", Self::offset(args).to_string()),
        ]
    }
}

impl SysPreadVHandle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn iov(args: &[usize]) -> *const IoVec {
        args[1] as *const IoVec
    }

    fn iov_count(args: &[usize]) -> usize {
        args[2]
    }

    fn offset(args: &[usize]) -> isize {
        args[3] as isize
    }
}

pub fn do_preadv(fd: i32, iovecs: &IoVecs, offset: isize) -> Result<usize, SystemError> {
    if offset < 0 {
        return Err(SystemError::EINVAL);
    }

    let binding = ProcessManager::current_pcb().fd_table();
    let fd_table_guard = binding.read();

    let file = fd_table_guard
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;

    drop(fd_table_guard);

    read_iovecs(file.as_ref(), iovecs, Some(offset as usize))
}

/// Complete one positional-vector read and publish ACCESS at the vector
/// syscall boundary. Linux `do_iter_read()` publishes on every nonnegative
/// completion, including EOF and an empty vector; scalar pread differs.
pub(super) fn read_iovecs(
    file: &File,
    iovecs: &IoVecs,
    offset: Option<usize>,
) -> Result<usize, SystemError> {
    // TODO: Support scatter-gather I/O directly in FS to avoid this copy.
    let mut data = iovecs.new_buf(true)?;
    let read_len = match offset {
        Some(offset) => file.pread_syscall_chunk(offset, data.len(), &mut data)?,
        None => file.read_syscall_chunk(data.len(), &mut data)?,
    };
    let copied = iovecs.scatter(&data[..read_len])?;
    file.notify_io_event(FsEvent::ACCESS);
    Ok(copied)
}

syscall_table_macros::declare_syscall!(SYS_PREADV, SysPreadVHandle);
