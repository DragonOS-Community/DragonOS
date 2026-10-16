use crate::arch::ipc::signal::Signal;
use crate::arch::syscall::nr::SYS_SPLICE;
use crate::filesystem::fsnotify::FsEvent;
use crate::filesystem::vfs::file::FileMode;
use crate::filesystem::vfs::FileFlags;
use crate::filesystem::vfs::{file::File, syscall::SpliceFlags, FileType};
use crate::ipc::kill::send_signal_to_pid;
use crate::ipc::pipe::PipeRuntime;
use crate::process::resource::RLimitID;
use crate::process::ProcessManager;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

// Linux uses MAX_RW_COUNT (typically 0x7ffff000) as the upper bound.
const MAX_RW_COUNT: usize = 0x7ffff000;

/// See <https://man7.org/linux/man-pages/man2/splice.2.html>
///
/// splice() 在两个文件描述符之间移动数据，其中一个必须是管道。
pub struct SysSpliceHandle;

impl Syscall for SysSpliceHandle {
    fn num_args(&self) -> usize {
        6
    }

    fn handle(
        &self,
        args: &[usize],
        _frame: &mut crate::arch::interrupt::TrapFrame,
    ) -> Result<usize, SystemError> {
        let fd_in = args[0] as i32;
        let off_in_ptr = args[1] as *mut i64;
        let fd_out = args[2] as i32;
        let off_out_ptr = args[3] as *mut i64;
        let len = args[4];
        let flags = args[5] as u32;

        // 参数校验
        if len == 0 {
            return Ok(0);
        }

        if len > MAX_RW_COUNT {
            return Err(SystemError::EINVAL);
        }

        // 解析标志位（Linux rejects unknown flags）
        let mut splice_flags = SpliceFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;

        // 读取输入偏移量（如果提供）
        let read_off_in = read_offset_from_user(off_in_ptr)?;

        // 读取输出偏移量（如果提供）
        let read_off_out = read_offset_from_user(off_out_ptr)?;

        // 获取文件对象
        let (file_in, file_out) = {
            let binding = ProcessManager::current_pcb().fd_table();
            let fd_table_guard = binding.read();

            let file_in = fd_table_guard
                .get_file_by_fd(fd_in)
                .ok_or(SystemError::EBADF)?;
            let file_out = fd_table_guard
                .get_file_by_fd(fd_out)
                .ok_or(SystemError::EBADF)?;
            (file_in.clone(), file_out.clone())
        };

        if !file_in.mode().contains(FileMode::FMODE_READ) {
            return Err(SystemError::EBADF);
        }
        if !file_out.mode().contains(FileMode::FMODE_WRITE) {
            return Err(SystemError::EBADF);
        }

        // 判断文件类型
        let in_is_pipe = is_pipe(&file_in);
        let out_is_pipe = is_pipe(&file_out);

        // Linux: inherit O_NONBLOCK from pipe endpoints only.
        // Regular files may have O_NONBLOCK but it does not affect splice behavior.
        if (in_is_pipe && file_in.flags().contains(FileFlags::O_NONBLOCK))
            || (out_is_pipe && file_out.flags().contains(FileFlags::O_NONBLOCK))
        {
            splice_flags.insert(SpliceFlags::SPLICE_F_NONBLOCK);
        }

        // 验证至少有一端是管道
        if !in_is_pipe && !out_is_pipe {
            return Err(SystemError::EINVAL);
        }

        // 验证偏移量指针只能用于常规文件
        if in_is_pipe && read_off_in.is_some() {
            return Err(SystemError::ESPIPE);
        }
        if out_is_pipe && read_off_out.is_some() {
            return Err(SystemError::ESPIPE);
        }

        // 检查是否为同一管道（Linux 6.6.21: do_splice 第 1263 行）
        // "Splicing to self would be fun, but..."
        if in_is_pipe && out_is_pipe && Arc::ptr_eq(&file_in.inode(), &file_out.inode()) {
            return Err(SystemError::EINVAL);
        }

        // 执行 splice
        let result = do_splice(
            &file_in,
            read_off_in,
            &file_out,
            read_off_out,
            len,
            splice_flags,
            in_is_pipe,
            out_is_pipe,
        )?;

        // 写回更新后的偏移量
        write_offset_to_user(off_out_ptr, read_off_out, result, out_is_pipe)?;
        write_offset_to_user(off_in_ptr, read_off_in, result, in_is_pipe)?;

        Ok(result)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<crate::syscall::table::FormattedSyscallParam> {
        vec![
            crate::syscall::table::FormattedSyscallParam::new("fd_in", format!("{:#x}", args[0])),
            crate::syscall::table::FormattedSyscallParam::new("off_in", format!("{:#x}", args[1])),
            crate::syscall::table::FormattedSyscallParam::new("fd_out", format!("{:#x}", args[2])),
            crate::syscall::table::FormattedSyscallParam::new("off_out", format!("{:#x}", args[3])),
            crate::syscall::table::FormattedSyscallParam::new("len", format!("{:#x}", args[4])),
            crate::syscall::table::FormattedSyscallParam::new("flags", format!("{:#x}", args[5])),
        ]
    }
}

/// 从用户空间读取偏移量
fn read_offset_from_user(off_ptr: *mut i64) -> Result<Option<usize>, SystemError> {
    if off_ptr.is_null() {
        return Ok(None);
    }

    let reader = UserBufferReader::new(off_ptr as *const i64, core::mem::size_of::<i64>(), true)?;
    let offset = reader.buffer_protected(0)?.read_one::<i64>(0)?;

    if offset < 0 {
        return Err(SystemError::EINVAL);
    }

    Ok(Some(offset as usize))
}

/// 将偏移量写回用户空间
fn write_offset_to_user(
    off_ptr: *mut i64,
    initial_offset: Option<usize>,
    transferred: usize,
    is_pipe: bool,
) -> Result<(), SystemError> {
    if off_ptr.is_null() || is_pipe {
        return Ok(());
    }

    if let Some(initial) = initial_offset {
        let new_off = initial + transferred;
        let mut writer = UserBufferWriter::new(off_ptr, core::mem::size_of::<i64>(), true)?;
        writer
            .buffer_protected(0)?
            .write_one::<i64>(0, &(new_off as i64))?;
    }

    Ok(())
}

/// 判断文件是否为管道
fn is_pipe(file: &File) -> bool {
    file.inode()
        .metadata()
        .map(|md| md.file_type == FileType::Pipe)
        .unwrap_or(false)
}

/// 执行 splice 操作的核心函数
#[allow(clippy::too_many_arguments)]
fn do_splice(
    file_in: &File,
    off_in: Option<usize>,
    file_out: &File,
    off_out: Option<usize>,
    len: usize,
    flags: SpliceFlags,
    in_is_pipe: bool,
    out_is_pipe: bool,
) -> Result<usize, SystemError> {
    let transferred = match (in_is_pipe, out_is_pipe) {
        (true, true) => splice_pipe_to_pipe(file_in, file_out, len, flags),
        (false, true) => splice_file_to_pipe(file_in, off_in, file_out, len, flags),
        (true, false) => splice_pipe_to_file(file_in, file_out, off_out, len, flags),
        (false, false) => unreachable!(),
    }?;

    if transferred > 0 {
        match (in_is_pipe, out_is_pipe) {
            (_, true) => {
                // The regular-file input path deliberately deferred ACCESS so
                // both pipe-output cases retain Linux's event order.
                file_out.notify_io_event(FsEvent::MODIFY);
                file_in.notify_io_event(FsEvent::ACCESS);
            }
            (true, false) => {
                // The held-pipe path publishes its actual prefix before sync,
                // including sync failure; do not duplicate those notifications.
            }
            (false, false) => unreachable!(),
        }
    }

    Ok(transferred)
}

fn get_pipe_inode(file: &File) -> Result<Arc<PipeRuntime>, SystemError> {
    file.pipe_runtime()
}

/// Linux 语义（fs/splice.c: ipipe_prep）：
/// - 输入 pipe 为空且仍有 writer：SPLICE_F_NONBLOCK -> EAGAIN
/// - 输入 pipe 为空且无 writer：返回 0 (EOF)
fn nonblock_prep_pipe_read(pipe_in: &File, flags: SpliceFlags) -> Result<(), SystemError> {
    if !flags.contains(SpliceFlags::SPLICE_F_NONBLOCK) {
        return Ok(());
    }

    let pipe_inode = get_pipe_inode(pipe_in)?;
    if pipe_inode.readable_len() == 0 && pipe_inode.has_writers() {
        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
    }
    // no writers => EOF; allow read path to return 0

    Ok(())
}

/// pipe 到 pipe 的数据传输
///
/// Transfer immediately available logical buffers up to the requested count.
/// Once progress is made, an empty input or full output returns that prefix.
fn splice_pipe_to_pipe(
    pipe_in: &File,
    pipe_out: &File,
    len: usize,
    flags: SpliceFlags,
) -> Result<usize, SystemError> {
    let in_pipe = get_pipe_inode(pipe_in)?;
    let out_pipe = get_pipe_inode(pipe_out)?;
    File::recover_io(|| in_pipe.splice_to_pipe(&out_pipe, len, flags))
}

/// file 到 pipe 的数据传输
fn splice_file_to_pipe(
    file: &File,
    offset: Option<usize>,
    pipe: &File,
    len: usize,
    flags: SpliceFlags,
) -> Result<usize, SystemError> {
    if offset.is_some()
        && (file.mode().contains(FileMode::FMODE_STREAM)
            || !file.mode().contains(FileMode::FMODE_PREAD))
    {
        return Err(SystemError::ESPIPE);
    }
    let mut position = offset.unwrap_or(0);
    splice_read_to_pipe(file, &mut position, offset.is_none(), pipe, len, flags)
}

/// Reserve pipe capacity before locking the implicit input position. Commit
/// that position before releasing the reservation; callers own notifications.
pub(super) fn splice_read_to_pipe(
    file: &File,
    position: &mut usize,
    implicit_position: bool,
    pipe: &File,
    len: usize,
    flags: SpliceFlags,
) -> Result<usize, SystemError> {
    File::recover_io(|| {
        try_splice_read_to_pipe(file, position, implicit_position, pipe, len, flags)
    })
}

fn try_splice_read_to_pipe(
    file: &File,
    position: &mut usize,
    implicit_position: bool,
    pipe: &File,
    len: usize,
    flags: SpliceFlags,
) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
    let pipe_inode = get_pipe_inode(pipe)?;
    let actor = crate::cgroup::memory::object_for_task(&ProcessManager::current_pcb());

    // A free logical slot admits an entire source page. Wait before taking
    // f_pos so a consumer can splice the full pipe back into this same OFD.
    let mut transaction = pipe_inode.begin_writer_transaction();
    let space = if flags.contains(SpliceFlags::SPLICE_F_NONBLOCK) {
        pipe_inode.writable_for_splice_nonblock(&transaction, None)?
    } else {
        pipe_inode.wait_writable_any_for_splice(&mut transaction)?
    };
    let _position_guard = if implicit_position {
        file.lock_position()
    } else {
        None
    };
    if implicit_position {
        *position = file.pos();
    }
    if len > isize::MAX as usize
        || position
            .checked_add(len)
            .filter(|end| *end <= isize::MAX as usize)
            .is_none()
    {
        return Err(SystemError::EINVAL.into());
    }
    let page_size = <crate::arch::MMArch as crate::mm::MemoryManagementArch>::PAGE_SIZE;
    let source_offset = if file.inode().page_cache().is_some() {
        *position & (page_size - 1)
    } else {
        0
    };
    let limit = len.min(MAX_RW_COUNT).min(page_size - source_offset);
    let buf_size = splice_trusted_file_read_limit(file, Some(*position), limit)
        .unwrap_or(limit)
        .min(space);
    // Even zero-count/EOF requests perform pipe readiness checks above.
    if buf_size == 0 {
        return Ok(0);
    }
    // A pipe has no splice_read operation. Zero count above needs no reader.
    if file.file_type() == FileType::Pipe {
        return Err(SystemError::EINVAL.into());
    }
    // All output inventory and the scratch are born before consuming input.
    pipe_inode.prepare_output_storage(&transaction, &actor)?;
    let mut buffer = crate::ipc::pipe::buffer::PipeBuffer::try_new_bytes(buf_size, &actor)?;

    // 从文件读取
    // 为了满足 Linux 语义：若后续写入 pipe 被信号中断且未写入任何字节，
    // 则不应推进输入文件的 file position。
    let read_len = file.try_read_at_for_transfer(*position, buf_size, &mut buffer)?;

    if read_len == 0 {
        return Ok(0);
    }

    // 写入 pipe
    let written =
        pipe_inode.write_from_splice_locked(&mut transaction, &buffer[..read_len], source_offset);

    match written {
        Ok(write_len) => {
            *position += write_len;
            if implicit_position {
                file.commit_transfer_position(*position);
            }
            Ok(write_len)
        }
        Err(e) => Err(e.into()),
    }
}

fn splice_trusted_file_read_limit(
    file: &File,
    offset: Option<usize>,
    limit: usize,
) -> Option<usize> {
    if limit == 0 {
        return Some(0);
    }

    if matches!(file.file_type(), FileType::File) && splice_regular_file_has_trusted_size(file) {
        if let Ok(metadata) = file.metadata() {
            let size = metadata.size.max(0) as usize;
            let pos = offset.unwrap_or_else(|| file.pos());
            return Some(limit.min(size.saturating_sub(pos)));
        }
    }

    None
}

fn splice_regular_file_has_trusted_size(file: &File) -> bool {
    file.with_io_fs(|fs| matches!(fs.name(), "ext4" | "fat" | "tmpfs" | "ramfs"))
}

/// pipe 到 file 的数据传输
fn splice_pipe_to_file(
    pipe: &File,
    file: &File,
    offset: Option<usize>,
    len: usize,
    flags: SpliceFlags,
) -> Result<usize, SystemError> {
    let pipe_inode = get_pipe_inode(pipe)?;
    let nonblock = flags.contains(SpliceFlags::SPLICE_F_NONBLOCK);

    // Fast-path Linux ipipe_prep(): NONBLOCK + empty (with writers) => EAGAIN.
    nonblock_prep_pipe_read(pipe, flags)?;

    // Preserve early EFBIG without consuming pipe data. The authoritative
    // check below uses the current position after the pipe becomes readable.
    splice_write_limit(file, offset.unwrap_or_else(|| file.pos()), len)?;
    let buf_size = len.min(4096);
    let mut buffer = File::recover_io(|| {
        let actor = crate::cgroup::memory::object_for_task(&ProcessManager::current_pcb());
        crate::ipc::pipe::buffer::PipeBuffer::try_new_bytes(buf_size, &actor)
    })?;
    let result = File::recover_io(|| {
        let read = pipe_inode.splice_peek_hold_from_blocking(buf_size, &mut buffer, nonblock)?;
        if read == 0 {
            return Ok(None);
        }
        let _position_guard = if offset.is_none() {
            file.lock_position()
        } else {
            None
        };
        let write_offset = offset.unwrap_or_else(|| file.pos());
        // Always release the logical hold, including errors in the position
        // dependent checks. No pipe bytes are consumed until a write succeeds.
        let attempt = (|| {
            let allowed = splice_write_limit(file, write_offset, read)?;
            file.try_write_for_transfer(write_offset, allowed, &buffer[..allowed], offset.is_none())
        })();
        match attempt {
            Ok(result) => {
                pipe_inode.splice_finish_hold(result.written_len);
                Ok(Some(result))
            }
            Err(error) => {
                pipe_inode.splice_finish_hold(0);
                Err(error)
            }
        }
    })?;
    match result {
        Some(result) => {
            if result.written_len > 0 {
                file.notify_io_event(FsEvent::MODIFY);
                pipe.notify_io_event(FsEvent::ACCESS);
            }
            file.finish_write(result)
        }
        None => Ok(0),
    }
}

/// Apply the regular-file size limit to a snapshotted output position.
fn splice_write_limit(file: &File, position: usize, len: usize) -> Result<usize, SystemError> {
    if file.file_type() != FileType::File {
        return Ok(len);
    }
    let current = ProcessManager::current_pcb();
    let limit = current.get_rlimit(RLimitID::Fsize).rlim_cur;
    if limit == u64::MAX {
        return Ok(len);
    }
    let limit = limit as usize;
    if position >= limit {
        if let Err(error) = send_signal_to_pid(current.raw_pid(), Signal::SIGXFSZ) {
            log::error!(
                "Failed to send SIGXFSZ for RLIMIT_FSIZE violation: {:?}",
                error
            );
        }
        return Err(SystemError::EFBIG);
    }
    Ok(len.min(limit - position))
}

syscall_table_macros::declare_syscall!(SYS_SPLICE, SysSpliceHandle);
