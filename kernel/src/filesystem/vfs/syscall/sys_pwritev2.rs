use alloc::{string::ToString, vec::Vec};

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_PWRITEV2;
use crate::filesystem::vfs::file::FileFlags;
use crate::filesystem::vfs::iov::{IoVec, IoVecs};
use crate::syscall::table::{FormattedSyscallParam, Syscall};

use super::sys_write::get_file_by_fd;

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

        let file = get_file_by_fd(fd)?;
        if offset == -1 {
            if file.flags().contains(FileFlags::O_PATH) {
                return Err(SystemError::EBADF);
            }
        } else {
            file.verify_pwrite_access()?;
        }

        // Import snapshots metadata and checks numerical ranges, without
        // materializing payload before the owner's write transaction.
        let iovecs = unsafe { IoVecs::from_user(iov, iov_count, false) }?;
        let append = flags.contains(RwfFlags::APPEND);
        if offset == -1 {
            file.write_user_vectored_with_append(&iovecs, append)
        } else {
            file.pwrite_user_vectored(offset as usize, &iovecs, append)
        }
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

syscall_table_macros::declare_syscall!(SYS_PWRITEV2, SysPwriteV2Handle);
