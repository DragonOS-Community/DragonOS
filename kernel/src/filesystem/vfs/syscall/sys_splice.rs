use crate::arch::syscall::nr::SYS_SPLICE;
use crate::filesystem::vfs::FileFlags;
use crate::filesystem::vfs::{file::File, syscall::SpliceFlags, FileType};
use crate::ipc::kill::send_signal_to_pid;
use crate::ipc::pipe::LockedPipeInode;
use crate::process::resource::RLimitID;
use crate::process::ProcessManager;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use crate::{arch::ipc::signal::Signal, libs::casting::DowncastArc};
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

        // 判断文件类型
        let in_is_pipe = is_pipe(&file_in);
        let out_is_pipe = is_pipe(&file_out);

        // Linux: inherit O_NONBLOCK from file descriptors.
        if file_in.flags().contains(FileFlags::O_NONBLOCK)
            || file_out.flags().contains(FileFlags::O_NONBLOCK)
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
    match (in_is_pipe, out_is_pipe) {
        (true, true) => splice_pipe_to_pipe(file_in, file_out, len, flags),
        (false, true) => splice_file_to_pipe(file_in, off_in, file_out, len, flags),
        (true, false) => splice_pipe_to_file(file_in, file_out, off_out, len, flags),
        (false, false) => unreachable!(),
    }
}

fn get_pipe_inode(file: &File) -> Result<Arc<LockedPipeInode>, SystemError> {
    let inode = file.inode();
    inode
        .downcast_arc::<LockedPipeInode>()
        .ok_or(SystemError::EBADF)
}

/// Linux 语义（fs/splice.c: ipipe_prep/opipe_prep）：
/// - 输入 pipe 为空且仍有 writer：SPLICE_F_NONBLOCK -> EAGAIN
/// - 输入 pipe 为空且无 writer：返回 0 (EOF)
/// - 输出 pipe 满且仍有 reader：SPLICE_F_NONBLOCK -> EAGAIN
/// - 输出 pipe 满且无 reader：写入端应触发 EPIPE（交由后续 write 路径处理）
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

fn nonblock_prep_pipe_write(pipe_out: &File, flags: SpliceFlags) -> Result<(), SystemError> {
    if !flags.contains(SpliceFlags::SPLICE_F_NONBLOCK) {
        return Ok(());
    }

    let pipe_inode = get_pipe_inode(pipe_out)?;
    if pipe_inode.writable_len() == 0 {
        // If there are no readers, Linux returns EPIPE (and SIGPIPE) rather than EAGAIN.
        // Let the write path handle that case.
        if pipe_inode.has_readers() {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
    }

    Ok(())
}

/// pipe 到 pipe 的数据传输
///
/// 关键行为：splice 只传输一个缓冲区单位的数据后就返回，
/// 不会循环传输直到达到 len。这是 Linux splice 的语义。
fn splice_pipe_to_pipe(
    pipe_in: &File,
    pipe_out: &File,
    len: usize,
    flags: SpliceFlags,
) -> Result<usize, SystemError> {
    let in_pipe = get_pipe_inode(pipe_in)?;
    let out_pipe = get_pipe_inode(pipe_out)?;
    in_pipe.splice_to_pipe(&out_pipe, len, flags)
}

/// file 到 pipe 的数据传输
fn splice_file_to_pipe(
    file: &File,
    offset: Option<usize>,
    pipe: &File,
    len: usize,
    flags: SpliceFlags,
) -> Result<usize, SystemError> {
    // Non-blocking semantics: if the output pipe is full, return EAGAIN instead of sleeping.
    nonblock_prep_pipe_write(pipe, flags)?;

    let buf_size = len.min(4096);
    let mut buffer = vec![0u8; buf_size];

    // 从文件读取
    // 为了满足 Linux 语义：若后续写入 pipe 被信号中断且未写入任何字节，
    // 则不应推进输入文件的 file position。
    let (read_len, advance_file_pos) = if let Some(off) = offset {
        (file.pread(off, buf_size, &mut buffer)?, false)
    } else {
        let off = file.pos();
        (file.pread(off, buf_size, &mut buffer)?, true)
    };

    if read_len == 0 {
        return Ok(0);
    }

    // 写入 pipe
    match pipe.write(read_len, &buffer[..read_len]) {
        Ok(write_len) => {
            if advance_file_pos {
                file.advance_pos(write_len);
            }
            Ok(write_len)
        }
        Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
            if flags.contains(SpliceFlags::SPLICE_F_NONBLOCK) =>
        {
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        }
        Err(e) => Err(e),
    }
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

    // RLIMIT_FSIZE (gVisor: FromPipeMaxFileSize) 语义：
    // 如果目标 regular file 的写入偏移已经到达/超过限制，则 splice 必须立刻失败 EFBIG，
    // 并且不能消耗 input pipe 中的数据。
    // 这里在“读取 pipe 之前”做预检查，避免先读后写失败导致 pipe 数据丢失。
    let mut allowed_len = len;
    if matches!(file.file_type(), FileType::File) {
        let write_offset = offset.unwrap_or_else(|| file.pos());
        let current_pcb = ProcessManager::current_pcb();
        let fsize_limit = current_pcb.get_rlimit(RLimitID::Fsize);
        if fsize_limit.rlim_cur != u64::MAX {
            let limit = fsize_limit.rlim_cur as usize;
            if write_offset >= limit {
                if let Err(e) = send_signal_to_pid(current_pcb.raw_pid(), Signal::SIGXFSZ) {
                    log::error!("Failed to send SIGXFSZ for RLIMIT_FSIZE violation: {:?}", e);
                }
                return Err(SystemError::EFBIG);
            }
            allowed_len = allowed_len.min(limit.saturating_sub(write_offset));
        }
    }

    let buf_size = allowed_len.min(4096);
    let mut buffer = vec![0u8; buf_size];

    // Read (consume) from pipe to avoid TOCTOU with concurrent readers.
    let read = pipe_inode.read_into_from_blocking(buf_size, &mut buffer, nonblock)?;
    if read == 0 {
        return Ok(0);
    }

    let written = if let Some(off) = offset {
        file.pwrite(off, read, &buffer[..read])
    } else {
        file.write(read, &buffer[..read])
    }?;

    if written == 0 {
        return Ok(0);
    }

    Ok(written)
}

syscall_table_macros::declare_syscall!(SYS_SPLICE, SysSpliceHandle);
