use alloc::string::ToString;
use alloc::vec::Vec;

use system_error::SystemError;

use crate::arch::syscall::nr::SYS_PREADV2;
use crate::filesystem::vfs::file::File;
use crate::filesystem::vfs::iov::{IoVec, IoVecs};
use crate::syscall::table::{FormattedSyscallParam, Syscall};

use super::sys_preadv::read_iovecs;
use super::sys_read::get_read_file;
use super::sys_readv::readv_from_user_with_flags;

// Linux 兼容的 RWF 标志位（preadv2 专用）
const RWF_HIPRI: u32 = 0x0000_0001;
const RWF_DSYNC: u32 = 0x0000_0002;
const RWF_SYNC: u32 = 0x0000_0004;
const RWF_NOWAIT: u32 = 0x0000_0008;
const RWF_APPEND: u32 = 0x0000_0010;
const RWF_VALID_MASK: u32 = RWF_HIPRI | RWF_DSYNC | RWF_SYNC | RWF_NOWAIT | RWF_APPEND;

/// Validate the operation flags at Linux's iterator/legacy dispatch boundary,
/// after admission, zero-length completion and signed range verification.
pub(super) fn validate_read_flags(file: &File, flags: u32) -> Result<(), SystemError> {
    if flags == 0 {
        return Ok(());
    }
    if flags & !RWF_VALID_MASK != 0 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    // There is no per-operation nonblocking contract through the current
    // backing read paths. Never accept NOWAIT and then perform a blocking read,
    // or emulate it by changing the shared open file description's flags.
    if flags & RWF_NOWAIT != 0 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    // Linux's legacy .read loop accepts only HIPRI. Iterator reads accept
    // DSYNC, SYNC and APPEND as well; these have no write effects on a read.
    if !file.supports_read_iter_flags() && flags & !RWF_HIPRI != 0 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    Ok(())
}

pub struct SysPreadV2Handle;

impl Syscall for SysPreadV2Handle {
    fn num_args(&self) -> usize {
        6
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

        if offset == -1 {
            return readv_from_user_with_flags(fd, iov, iov_count, flags);
        }

        // fdget and FMODE_PREAD precede import_iovec, while READ/CAN_READ
        // and RWF checks belong after importing the complete vector snapshot.
        let file = get_read_file(fd)?;
        file.verify_pread_access()?;
        let iovecs = unsafe { IoVecs::from_user(iov, iov_count, true) }?;
        read_iovecs(file.as_ref(), &iovecs, offset as usize, flags)
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
        // All supported architectures use the native 64-bit Linux ABI:
        // pos_l is a full word; pos_h occupies args[4] but is ignored.
        args[3] as isize
    }

    fn flags(args: &[usize]) -> u32 {
        // Linux rwf_t is 32 bits even in the native 64-bit syscall ABI.
        args[5] as u32
    }
}

/// preadv2 的核心实现
///
/// - `offset == -1` 时行为等同于 `readv`，会推进文件偏移量
/// - 其它非负 offset 走已有的 `preadv` 实现，不会修改文件偏移量
syscall_table_macros::declare_syscall!(SYS_PREADV2, SysPreadV2Handle);
