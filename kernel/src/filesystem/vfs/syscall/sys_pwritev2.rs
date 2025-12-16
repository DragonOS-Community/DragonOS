use alloc::{string::ToString, sync::Arc, vec::Vec};

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_PWRITEV2;
use crate::filesystem::vfs::file::File;
use crate::filesystem::vfs::iov::{IoVec, IoVecs};
use crate::filesystem::vfs::FileType;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};

use super::sys_pwrite64::validate_pwrite_range;

bitflags::bitflags! {
    /// Linux 兼容的 RWF 标志位（pwritev2 专用）
    pub struct RwfFlags: usize {
        const HIPRI  = 0x0000_0001;
        const DSYNC  = 0x0000_0002;
        const SYNC   = 0x0000_0004;
        const NOWAIT = 0x0000_0008;
        const APPEND = 0x0000_0010;
    }
}

pub struct SysPwriteV2Handle;

impl Syscall for SysPwriteV2Handle {
    fn num_args(&self) -> usize {
        #[cfg(target_arch = "x86_64")]
        {
            6
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            5
        }
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let iov = Self::iov(args);
        let iov_count = Self::iov_count(args);
        let offset = Self::offset(args);
        let raw_flags = Self::flags(args);
        // 未知标志返回 EOPNOTSUPP，遵循 Linux 语义。
        let flags = RwfFlags::from_bits(raw_flags).ok_or(SystemError::EOPNOTSUPP_OR_ENOTSUP)?;

        // 先做基础的 offset 校验，-1 允许，其余负值返回 EINVAL
        if offset < -1 {
            return Err(SystemError::EINVAL);
        }

        // 先检查 fd 合法性，以确保 iovcnt==0 且 fd 非法时仍返回 EBADF（与 Linux 一致）
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;
        drop(fd_table_guard);

        // 构造 IoVecs（会验证用户缓冲区可读性）
        let iovecs = unsafe { IoVecs::from_user(iov, iov_count, false) }?;
        let data = iovecs.gather()?;

        do_pwritev2(file, offset, flags, data)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd:", Self::fd(args).to_string()),
            FormattedSyscallParam::new("iov:", format!("{:#x}", Self::iov(args) as usize)),
            FormattedSyscallParam::new("iov_count:", Self::iov_count(args).to_string()),
            FormattedSyscallParam::new("offset:", Self::offset(args).to_string()),
            FormattedSyscallParam::new("flags:", format!("{:#x}", Self::flags(args))),
        ]
    }
}

impl SysPwriteV2Handle {
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
        #[cfg(target_arch = "x86_64")]
        {
            let lo = args[3] as u64;
            let hi = args[4] as u64;
            let combined = (hi << 32) | lo;
            combined as i64 as isize
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            args[3] as isize
        }
    }

    fn flags(args: &[usize]) -> usize {
        #[cfg(target_arch = "x86_64")]
        {
            args[5]
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            args[4]
        }
    }
}

/// pwritev2 的核心实现
///
/// - `offset == -1` 时行为等同于 `writev`，会推进文件偏移量
/// - 其它非负 offset 走已有的 pwrite 语义，不会修改文件偏移量
pub fn do_pwritev2(
    file: Arc<File>,
    offset: isize,
    flags: RwfFlags,
    data: Vec<u8>,
) -> Result<usize, SystemError> {
    // offset == -1 -> 使用当前文件偏移（行为与 writev 相同）
    if offset == -1 {
        // RWF_APPEND：强制追加写入（需满足“取 EOF + 写入”的原子性），并推进文件偏移
        if flags.contains(RwfFlags::APPEND) {
            return file.write_append(data.len(), &data);
        }
        return file.write(data.len(), &data);
    }

    // 非负 offset：不允许作用于管道/Socket，保持与 pwrite 语义一致
    let md = file.metadata()?;
    if md.file_type == FileType::Pipe || md.file_type == FileType::Socket {
        return Err(SystemError::ESPIPE);
    }

    // offset 为非负时，执行范围校验
    let offset = validate_pwrite_range(offset as i64, data.len())?;

    // 若指定 RWF_APPEND，忽略 offset，改为在文件末尾写入，但不更新文件偏移
    if flags.contains(RwfFlags::APPEND) {
        return file.pwrite_append(offset, data.len(), &data);
    }

    // 普通 pwrite 路径
    file.pwrite(offset, data.len(), &data)
}

syscall_table_macros::declare_syscall!(SYS_PWRITEV2, SysPwriteV2Handle);
