use alloc::string::ToString;
use alloc::vec::Vec;

use system_error::SystemError;

use crate::arch::syscall::nr::SYS_PREADV;
use crate::filesystem::vfs::file::File;
use crate::filesystem::vfs::iov::{IoVec, IoVecs};
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};

pub struct SysPreadVHandle;

impl Syscall for SysPreadVHandle {
    fn num_args(&self) -> usize {
        4
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
        if offset < 0 {
            return Err(SystemError::EINVAL);
        }

        let file = super::sys_read::get_read_file(fd)?;
        file.verify_pread_access()?;
        let iovecs = unsafe { IoVecs::from_user(iov, iov_count, true) }?;
        read_iovecs(file.as_ref(), &iovecs, offset as usize, 0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd:", Self::fd(args).to_string()),
            FormattedSyscallParam::new("iov:", format!("{:#x}", Self::iov(args) as usize)),
            FormattedSyscallParam::new("iov_count:", Self::iov_count(args).to_string()),
            FormattedSyscallParam::new("offset:", Self::offset(args).to_string()),
        ]
    }
}

impl SysPreadVHandle {
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
        args[3] as isize
    }
}

/// Complete one positional-vector read and publish ACCESS at the vector
/// syscall boundary. Linux `do_iter_read()` publishes on every nonnegative
/// completion, including EOF and an empty vector; scalar pread differs.
pub(super) fn read_iovecs(
    file: &File,
    iovecs: &IoVecs,
    offset: usize,
    flags: u32,
) -> Result<usize, SystemError> {
    super::sys_readv::read_iovecs_at(file, iovecs, flags, Some(offset))
}

syscall_table_macros::declare_syscall!(SYS_PREADV, SysPreadVHandle);
