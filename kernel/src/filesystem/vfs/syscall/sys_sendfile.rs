use crate::arch::syscall::nr::SYS_SENDFILE;
use crate::filesystem::fsnotify::FsEvent;
use crate::filesystem::vfs::{file::FileMode, syscall::SpliceFlags, FileFlags, FileType};
use crate::process::ProcessManager;
use crate::syscall::table::Syscall;
use crate::syscall::user_buffer::UserBuffer;
use alloc::vec::Vec;
use system_error::SystemError;

const MAX_RW_COUNT: usize = 0x7ffff000;

/// See <https://man7.org/linux/man-pages/man2/sendfile64.2.html>
pub struct SysSendfileHandle;

impl Syscall for SysSendfileHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(
        &self,
        args: &[usize],
        _frame: &mut crate::arch::interrupt::TrapFrame,
    ) -> Result<usize, SystemError> {
        let pointer = args[2] as *mut i64;
        let mut user_offset = if pointer.is_null() {
            None
        } else {
            Some(UserBuffer::new_protected(pointer, size_of::<i64>(), true)?)
        };
        let mut position = match user_offset.as_ref() {
            Some(buffer) => buffer.read_one::<i64>(0)?,
            None => 0,
        };
        let result = sendfile_transfer(
            args[0] as i32,
            args[1] as i32,
            &mut position,
            user_offset.is_some(),
            args[3],
        );
        // Linux copies the cursor back even after an error or zero progress.
        // Do not preflight write permissions: data may already have been sent
        // when this final copy faults, and EFAULT overrides the core result.
        if let Some(buffer) = user_offset.as_mut() {
            buffer.write_one(0, &position)?;
        }
        result
    }

    fn entry_format(&self, args: &[usize]) -> Vec<crate::syscall::table::FormattedSyscallParam> {
        vec![
            crate::syscall::table::FormattedSyscallParam::new("out_fd", format!("{:#x}", args[0])),
            crate::syscall::table::FormattedSyscallParam::new("in_fd", format!("{:#x}", args[1])),
            crate::syscall::table::FormattedSyscallParam::new("offset", format!("{:#x}", args[2])),
            crate::syscall::table::FormattedSyscallParam::new("count", format!("{:#x}", args[3])),
        ]
    }
}

/// Own the transfer cursors, not the file-table lock or a new pair of f_pos
/// locks. Linux do_sendfile uses fdget rather than fdget_pos as well.
fn sendfile_transfer(
    out_fd: i32,
    in_fd: i32,
    position: &mut i64,
    explicit_offset: bool,
    count: usize,
) -> Result<usize, SystemError> {
    let table = ProcessManager::current_pcb().fd_table();
    let input = table
        .read()
        .get_file_by_fd(in_fd)
        .ok_or(SystemError::EBADF)?;
    input.readable()?;
    if explicit_offset && !input.mode().contains(FileMode::FMODE_PREAD) {
        return Err(SystemError::ESPIPE);
    }
    let mut input_position = if explicit_offset {
        *position as usize
    } else {
        input.pos()
    };
    // Verify the original count before MAX_RW_COUNT truncation, as rw_verify_area does.
    if (explicit_offset && *position < 0)
        || count > isize::MAX as usize
        || input_position
            .checked_add(count)
            .filter(|end| *end <= i64::MAX as usize)
            .is_none()
    {
        return Err(SystemError::EINVAL);
    }
    let count = count.min(MAX_RW_COUNT);
    let output = table
        .read()
        .get_file_by_fd(out_fd)
        .ok_or(SystemError::EBADF)?;
    output.writeable()?;
    if output.file_type() == FileType::Pipe {
        let flags = if output.flags().contains(FileFlags::O_NONBLOCK) {
            SpliceFlags::SPLICE_F_NONBLOCK
        } else {
            SpliceFlags::empty()
        };
        let written = super::sys_splice::splice_read_to_pipe(
            &input,
            &mut input_position,
            &output,
            count,
            flags,
        )?;
        if written > 0 {
            if explicit_offset {
                *position = input_position as i64;
            } else {
                input.commit_transfer_position(input_position);
            }
            input.notify_io_event(FsEvent::ACCESS);
            output.notify_io_event(FsEvent::MODIFY);
        }
        return Ok(written);
    }
    let output_stream = output.mode().contains(FileMode::FMODE_STREAM);
    let mut output_position = if output_stream { 0 } else { output.pos() };
    if output_position
        .checked_add(count)
        .filter(|end| *end <= i64::MAX as usize)
        .is_none()
    {
        return Err(SystemError::EINVAL);
    }
    if output.flags().contains(FileFlags::O_APPEND) {
        return Err(SystemError::EINVAL);
    }
    // A consumed stream cannot be rewound after a short output write.
    if !input.mode().contains(FileMode::FMODE_LSEEK) {
        return Err(SystemError::EINVAL);
    }
    if count == 0 {
        return Ok(0);
    }

    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(4096)
        .map_err(|_| SystemError::ENOMEM)?;
    buffer.resize(4096, 0);
    let mut transferred = 0;
    let result = loop {
        let requested = buffer.len().min(count - transferred);
        let read =
            match input.read_at_for_transfer(input_position, requested, &mut buffer[..requested]) {
                Ok(0) => break Ok(transferred),
                Ok(read) => read,
                Err(error) => {
                    break if transferred > 0 {
                        Ok(transferred)
                    } else {
                        Err(error)
                    }
                }
            };
        let written = match output.write_for_transfer(output_position, read, &buffer[..read], false)
        {
            Ok(written) => written,
            Err(error) => {
                break if transferred > 0 {
                    Ok(transferred)
                } else {
                    Err(error)
                }
            }
        };
        transferred += written;
        input_position += written;
        if !output_stream {
            output_position += written;
        }
        if transferred == count || written < read {
            break Ok(transferred);
        }
    };

    if transferred > 0 {
        // Ordering matters when both descriptors share the same open file:
        // the implicit input cursor is the final f_pos, never two additions.
        output.commit_transfer_position(output_position);
        if explicit_offset {
            *position = input_position as i64;
        } else {
            input.commit_transfer_position(input_position);
        }
        input.notify_io_event(FsEvent::ACCESS);
        output.notify_io_event(FsEvent::MODIFY);
    }
    result
}

syscall_table_macros::declare_syscall!(SYS_SENDFILE, SysSendfileHandle);
