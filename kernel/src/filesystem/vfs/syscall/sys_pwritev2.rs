use alloc::{string::ToString, vec::Vec};

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_PWRITEV2;
use crate::filesystem::vfs::file::FileFlags;
use crate::filesystem::vfs::iov::{IoVec, IoVecs};
use crate::syscall::table::{FormattedSyscallParam, Syscall};

use super::sys_write::get_file_by_fd;

pub struct SysPwriteV2Handle;

impl Syscall for SysPwriteV2Handle {
    fn num_args(&self) -> usize {
        6
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let iov = Self::iov(args);
        let iov_count = Self::iov_count(args);
        let offset = Self::offset(args);
        let flags = Self::flags(args);

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
        if offset == -1 {
            file.write_user_vectored_with_flags(&iovecs, flags)
        } else {
            file.pwrite_user_vectored_with_flags(offset as usize, &iovecs, flags)
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
        // Native 64-bit Linux pos_from_hilo() retains the full low word.
        // The high-word ABI slot is present on every architecture but ignored.
        args[3] as isize
    }

    fn flags(args: &[usize]) -> u32 {
        // rwf_t is 32 bits even in the native 64-bit syscall ABI.
        args[5] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_PWRITEV2, SysPwriteV2Handle);
