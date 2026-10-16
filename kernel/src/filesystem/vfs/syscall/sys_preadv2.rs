use alloc::string::ToString;
use alloc::vec::Vec;

use system_error::SystemError;

use crate::arch::syscall::nr::SYS_PREADV2;
use crate::filesystem::vfs::iov::{IoVec, IoVecs};
use crate::syscall::table::{FormattedSyscallParam, Syscall};

use super::sys_preadv::do_preadv;
use super::sys_readv::readv_from_user;

// Linux 兼容的 RWF 标志位（preadv2 专用）
const RWF_HIPRI: usize = 0x0000_0001;
const RWF_DSYNC: usize = 0x0000_0002;
const RWF_SYNC: usize = 0x0000_0004;
const RWF_NOWAIT: usize = 0x0000_0008;
const RWF_APPEND: usize = 0x0000_0010;
const RWF_VALID_MASK: usize = RWF_HIPRI | RWF_DSYNC | RWF_SYNC | RWF_NOWAIT | RWF_APPEND;

pub struct SysPreadV2Handle;

impl Syscall for SysPreadV2Handle {
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

    fn handle(
        &self,
        args: &[usize],
        _frame: &mut crate::arch::interrupt::TrapFrame,
    ) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let iov = Self::iov(args);
        let iov_count = Self::iov_count(args);
        let offset = Self::offset(args);
        let flags = Self::flags(args);
        if offset < -1 {
            return Err(SystemError::EINVAL);
        }

        // 先校验标志位，遵循 Linux 语义：未知标志返回 EOPNOTSUPP。
        if flags & !RWF_VALID_MASK != 0 {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        if offset == -1 {
            return readv_from_user(fd, iov, iov_count);
        }

        // 构造 IoVecs（会验证用户缓冲区可写性）
        let iovecs = unsafe { IoVecs::from_user(iov, iov_count, true) }?;

        do_preadv(fd, &iovecs, offset)
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

impl SysPreadV2Handle {
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

/// preadv2 的核心实现
///
/// - `offset == -1` 时行为等同于 `readv`，会推进文件偏移量
/// - 其它非负 offset 走已有的 `preadv` 实现，不会修改文件偏移量
syscall_table_macros::declare_syscall!(SYS_PREADV2, SysPreadV2Handle);
