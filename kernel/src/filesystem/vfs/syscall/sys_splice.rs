//! System call handler for sys_splice.

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_SPLICE},
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
    syscall::user_access::UserBufferReader,
    syscall::user_access::UserBufferWriter,
};
use alloc::vec::Vec;
use system_error::SystemError;

/// Maximum length for splice operation (1GB)
const MAX_SPLICE_LEN: usize = 1024 * 1024 * 1024;
/// Size of the offset pointer (i64)
const OFFSET_SIZE: usize = core::mem::size_of::<i64>();
/// Chunk size for data copy (64KB)
const COPY_CHUNK_SIZE: usize = 64 * 1024;

pub struct SysSpliceHandle;

impl Syscall for SysSpliceHandle {
    fn num_args(&self) -> usize {
        6
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd_in = args[0] as i32;
        let off_in_ptr = args[1] as *mut i64;
        let fd_out = args[2] as i32;
        let off_out_ptr = args[3] as *mut i64;
        let len = args[4];
        let _flags = args[5] as u32;

        if len == 0 {
            return Ok(0);
        }

        if len > MAX_SPLICE_LEN {
            return Err(SystemError::EINVAL);
        }

        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        let file_in = fd_table_guard
            .get_file_by_fd(fd_in)
            .ok_or(SystemError::EBADF)?;
        let file_out = fd_table_guard
            .get_file_by_fd(fd_out)
            .ok_or(SystemError::EBADF)?;

        // Check for pipe
        let in_is_pipe = file_in.inode().is_stream();
        let out_is_pipe = file_out.inode().is_stream();

        if !in_is_pipe && !out_is_pipe {
            return Err(SystemError::EINVAL);
        }

        // We drop the guard to allow concurrent operations if needed, but for now we just hold refs.
        // Actually we need to keep files alive.
        let file_in = file_in.clone();
        let file_out = file_out.clone();
        drop(fd_table_guard);

        let mut off_in = read_user_offset(off_in_ptr)?;
        let mut off_out = read_user_offset(off_out_ptr)?;

        // Validation: if ESPIPE, cannot use offset
        if in_is_pipe && off_in.is_some() {
            return Err(SystemError::ESPIPE);
        }
        if out_is_pipe && off_out.is_some() {
            return Err(SystemError::ESPIPE);
        }

        // Buffer for copy
        // Use a reasonable buffer size, loop if needed
        let chunk_size = core::cmp::min(len, COPY_CHUNK_SIZE);
        let mut buf = alloc::vec![0u8; chunk_size];

        let mut total_transferred = 0;
        let mut remaining = len;

        while remaining > 0 {
            let to_read = core::cmp::min(remaining, chunk_size);

            // Read from in
            let read_res = if let Some(off) = off_in {
                file_in.pread(off, to_read, &mut buf[..to_read])
            } else {
                file_in.read(to_read, &mut buf[..to_read])
            };

            let bytes_read = match read_res {
                Ok(n) => n,
                Err(e) => {
                    if total_transferred > 0 {
                        break;
                    }
                    return Err(e);
                }
            };

            if bytes_read == 0 {
                break;
            }

            if let Some(off) = off_in.as_mut() {
                *off += bytes_read;
            }

            // Write to out
            let write_res = if let Some(off) = off_out {
                file_out.pwrite(off, bytes_read, &buf[..bytes_read])
            } else {
                file_out.write(bytes_read, &buf[..bytes_read])
            };

            let bytes_written = match write_res {
                Ok(n) => n,
                Err(e) => {
                    if total_transferred > 0 {
                        break;
                    }
                    return Err(e);
                }
            };

            if let Some(off) = off_out.as_mut() {
                *off += bytes_written;
            }

            total_transferred += bytes_written;
            remaining -= bytes_written;

            if bytes_written < bytes_read {
                // Short write
                break;
            }
        }

        // Update offsets if needed
        if let Some(off) = off_in {
            write_user_offset(off_in_ptr, off)?;
        }

        if let Some(off) = off_out {
            write_user_offset(off_out_ptr, off)?;
        }

        Ok(total_transferred)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd_in", format!("{}", args[0] as i32)),
            FormattedSyscallParam::new("off_in", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("fd_out", format!("{}", args[2] as i32)),
            FormattedSyscallParam::new("off_out", format!("{:#x}", args[3])),
            FormattedSyscallParam::new("len", format!("{}", args[4])),
            FormattedSyscallParam::new("flags", format!("{:#x}", args[5])),
        ]
    }
}

/// Helper to safely read offset from user space using buffer_protected
fn read_user_offset(ptr: *mut i64) -> Result<Option<usize>, SystemError> {
    if ptr.is_null() {
        return Ok(None);
    }
    let reader = UserBufferReader::new(ptr, OFFSET_SIZE, true)?;
    let val = reader.buffer_protected(0)?.read_one::<i64>(0)?;
    Ok(Some(val as usize))
}

/// Helper to safely write offset to user space using buffer_protected
fn write_user_offset(ptr: *mut i64, val: usize) -> Result<(), SystemError> {
    if ptr.is_null() {
        return Ok(());
    }
    let mut writer = UserBufferWriter::new(ptr, OFFSET_SIZE, true)?;
    writer
        .buffer_protected(0)?
        .write_to_user(0, &(val as i64).to_ne_bytes())?;
    Ok(())
}

syscall_table_macros::declare_syscall!(SYS_SPLICE, SysSpliceHandle);
