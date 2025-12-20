//! copy_file_range 系统调用实现
//!
//! 参考: Linux 6.6.21 fs/read_write.c
//! 文档: https://man7.org/linux/man-pages/man2/copy_file_range.2.html

use crate::arch::syscall::nr::SYS_COPY_FILE_RANGE;
use crate::filesystem::vfs::file::{File, FileFlags, FileMode};
use crate::filesystem::vfs::FileType;
use crate::process::ProcessManager;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;
use system_error::SystemError;

/// copy_file_range 系统调用处理结构体
pub struct SysCopyFileRangeHandle;

impl Syscall for SysCopyFileRangeHandle {
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

        log::trace!(
            "copy_file_range: fd_in={}, off_in_ptr={:?}, fd_out={}, off_out_ptr={:?}, len={}, flags={}",
            fd_in, off_in_ptr, fd_out, off_out_ptr, len, flags
        );

        do_copy_file_range(fd_in, off_in_ptr, fd_out, off_out_ptr, len, flags)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<crate::syscall::table::FormattedSyscallParam> {
        vec![
            crate::syscall::table::FormattedSyscallParam::new(
                "fd_in",
                format!("{}", args[0] as i32),
            ),
            crate::syscall::table::FormattedSyscallParam::new("off_in", format!("{:#x}", args[1])),
            crate::syscall::table::FormattedSyscallParam::new(
                "fd_out",
                format!("{}", args[2] as i32),
            ),
            crate::syscall::table::FormattedSyscallParam::new("off_out", format!("{:#x}", args[3])),
            crate::syscall::table::FormattedSyscallParam::new("len", format!("{:#x}", args[4])),
            crate::syscall::table::FormattedSyscallParam::new("flags", format!("{:#x}", args[5])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_COPY_FILE_RANGE, SysCopyFileRangeHandle);

/// 执行 copy_file_range 系统调用
///
/// # 参数
/// - `fd_in`: 源文件描述符
/// - `off_in_ptr`: 源文件偏移指针（NULL 表示使用文件当前偏移）
/// - `fd_out`: 目标文件描述符
/// - `off_out_ptr`: 目标文件偏移指针（NULL 表示使用文件当前偏移）
/// - `len`: 要拷贝的字节数
/// - `flags`: 标志位（目前必须为 0）
fn do_copy_file_range(
    fd_in: i32,
    off_in_ptr: *mut i64,
    fd_out: i32,
    off_out_ptr: *mut i64,
    len: usize,
    flags: u32,
) -> Result<usize, SystemError> {
    // Linux 6.6: flags 必须为 0
    if flags != 0 {
        return Err(SystemError::EINVAL);
    }

    // 获取文件对象
    let (in_file, out_file) = {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let in_file = fd_table_guard
            .get_file_by_fd(fd_in)
            .ok_or(SystemError::EBADF)?;
        let out_file = fd_table_guard
            .get_file_by_fd(fd_out)
            .ok_or(SystemError::EBADF)?;
        (in_file, out_file)
    };

    // 文件类型和权限检查
    generic_file_rw_checks(&in_file, &out_file)?;

    // 读取用户空间的偏移量
    let pos_in = read_offset_from_user(off_in_ptr)?;
    let pos_out = read_offset_from_user(off_out_ptr)?;

    // 执行拷贝
    let copied = copy_file_range_impl(
        &in_file,
        off_in_ptr.is_null(),
        pos_in,
        &out_file,
        off_out_ptr.is_null(),
        pos_out,
        len,
    )?;

    // 如果有拷贝的字节，更新用户空间的偏移
    if copied > 0 {
        if !off_in_ptr.is_null() {
            write_offset_to_user(off_in_ptr, pos_in.map(|p| p + copied))?;
        }
        if !off_out_ptr.is_null() {
            write_offset_to_user(off_out_ptr, pos_out.map(|p| p + copied))?;
        }
    }

    Ok(copied)
}

/// 通用文件读写检查（参考 Linux generic_file_rw_checks）
fn generic_file_rw_checks(file_in: &Arc<File>, file_out: &Arc<File>) -> Result<(), SystemError> {
    let md_in = file_in.metadata()?;
    let md_out = file_out.metadata()?;

    // 不允许拷贝目录
    if md_in.file_type == FileType::Dir || md_out.file_type == FileType::Dir {
        return Err(SystemError::EISDIR);
    }

    // 只支持普通文件
    if md_in.file_type != FileType::File || md_out.file_type != FileType::File {
        return Err(SystemError::EINVAL);
    }

    // 检查源文件读权限
    let mode_in = file_in.mode();
    if !mode_in.contains(FileMode::FMODE_READ) {
        return Err(SystemError::EBADF);
    }

    // 检查目标文件写权限
    let mode_out = file_out.mode();
    if !mode_out.contains(FileMode::FMODE_WRITE) {
        return Err(SystemError::EBADF);
    }

    // 目标文件不能是 O_APPEND 模式
    if file_out.flags().contains(FileFlags::O_APPEND) {
        return Err(SystemError::EBADF);
    }

    Ok(())
}

/// 从用户空间读取偏移量
///
/// 返回 Some(offset) 如果指针非空，返回 None 如果指针为空（表示使用文件当前偏移）
fn read_offset_from_user(off_ptr: *mut i64) -> Result<Option<usize>, SystemError> {
    if off_ptr.is_null() {
        return Ok(None);
    }

    let reader = UserBufferReader::new(off_ptr as *const i64, size_of::<i64>(), true)?;
    let offset = reader.buffer_protected(0)?.read_one::<i64>(0)?;

    if offset < 0 {
        return Err(SystemError::EINVAL);
    }

    Ok(Some(offset as usize))
}

/// 将偏移量写回用户空间
fn write_offset_to_user(off_ptr: *mut i64, offset: Option<usize>) -> Result<(), SystemError> {
    if off_ptr.is_null() || offset.is_none() {
        return Ok(());
    }

    let offset_val = offset.unwrap() as i64;
    let mut writer = UserBufferWriter::new(off_ptr, size_of::<i64>(), true)?;
    writer.buffer_protected(0)?.write_one(0, &offset_val)?;
    Ok(())
}

/// 核心拷贝实现
///
/// # 参数
/// - `in_file`: 源文件
/// - `use_in_file_offset`: 是否使用源文件的当前偏移（off_in 为 NULL）
/// - `pos_in`: 如果 use_in_file_offset 为 false，则为用户指定的偏移
/// - `out_file`: 目标文件
/// - `use_out_file_offset`: 是否使用目标文件的当前偏移（off_out 为 NULL）
/// - `pos_out`: 如果 use_out_file_offset 为 false，则为用户指定的偏移
/// - `len`: 要拷贝的字节数
fn copy_file_range_impl(
    in_file: &Arc<File>,
    use_in_file_offset: bool,
    pos_in: Option<usize>,
    out_file: &Arc<File>,
    use_out_file_offset: bool,
    pos_out: Option<usize>,
    len: usize,
) -> Result<usize, SystemError> {
    // 获取源文件元数据
    let md_in = in_file.metadata()?;
    let size_in = md_in.size;

    // 如果长度为 0，直接返回
    if len == 0 {
        return Ok(0);
    }

    // 获取起始偏移
    // 注意：如果 use_in/out_file_offset 为 true，这里的 start_pos_in/out (0) 仅用于溢出检查占位，
    // 实际并不代表文件当前偏移。
    let start_pos_in = pos_in.unwrap_or(0);
    let start_pos_out = pos_out.unwrap_or(0);

    // 检查偏移溢出
    start_pos_in
        .checked_add(len)
        .ok_or(SystemError::EOVERFLOW)?;
    start_pos_out
        .checked_add(len)
        .ok_or(SystemError::EOVERFLOW)?;

    if size_in < 0 {
        return Err(SystemError::EINVAL);
    }
    let size_in = size_in as usize;

    // 如果起始位置已经超过文件大小，返回 0
    // 注意：如果 use_in_file_offset 为 true，我们依赖 in_file.read() 返回 0 来处理 EOF，
    // 因为此时我们不知道文件的当前偏移量。
    if !use_in_file_offset && start_pos_in >= size_in {
        return Ok(0);
    }

    // 计算实际可读取的长度
    let actual_len = if use_in_file_offset {
        // 使用文件当前偏移时，不能预先裁剪，因为我们不知道当前偏移
        len
    } else {
        len.min(size_in.saturating_sub(start_pos_in))
    };

    if actual_len == 0 {
        return Ok(0);
    }

    // 检查同一文件的重叠写入
    // 使用 metadata 的 inode_id 和 dev_id 来判断是否是同一文件
    let md_out = out_file.metadata()?;
    if md_in.inode_id == md_out.inode_id && md_in.dev_id == md_out.dev_id {
        // 同一文件，检查是否有重叠
        // 注意：只有在两个偏移都是已知的情况下才能检查
        if !use_in_file_offset
            && !use_out_file_offset
            && overlaps(start_pos_in, actual_len, start_pos_out, actual_len)
        {
            return Err(SystemError::EINVAL);
        }
    }

    // 使用 4KB 缓冲区循环拷贝
    const BUF_SIZE: usize = 4096;
    let mut buffer = vec![0u8; BUF_SIZE].into_boxed_slice();
    let mut total_copied: usize = 0;
    let mut current_pos_in = start_pos_in;
    let mut current_pos_out = start_pos_out;

    while total_copied < actual_len {
        let remaining = actual_len - total_copied;
        let to_copy = remaining.min(BUF_SIZE);

        // 读取数据
        let read_len = if use_in_file_offset {
            // 使用文件当前偏移读取，自动更新文件偏移
            in_file.read(to_copy, &mut buffer[..to_copy])?
        } else {
            // 使用指定偏移读取，不更新文件偏移
            in_file.do_read(current_pos_in, to_copy, &mut buffer[..to_copy], false)?
        };

        if read_len == 0 {
            break; // EOF
        }

        // 写入数据
        let written = if use_out_file_offset {
            // 使用文件当前偏移写入，自动更新文件偏移
            out_file.write(read_len, &buffer[..read_len])?
        } else {
            // 使用指定偏移写入，不更新文件偏移
            out_file.do_write(current_pos_out, read_len, &buffer[..read_len], false, false)?
        };

        total_copied += written;
        current_pos_in += written;
        current_pos_out += written;

        if written < read_len {
            break; // 短写
        }
    }

    Ok(total_copied)
}

/// 检查两个范围是否重叠
#[inline]
fn overlaps(start1: usize, len1: usize, start2: usize, len2: usize) -> bool {
    let end1 = start1.saturating_add(len1);
    let end2 = start2.saturating_add(len2);
    start1 < end2 && start2 < end1
}
