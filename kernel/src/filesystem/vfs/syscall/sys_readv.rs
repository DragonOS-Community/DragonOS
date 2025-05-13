use system_error::SystemError;

use crate::arch::syscall::nr::SYS_READV;
use crate::filesystem::vfs::iov::IoVec;
use crate::filesystem::vfs::iov::IoVecs;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;

use alloc::string::ToString;
use alloc::vec::Vec;

use super::sys_read::do_read;

/// System call handler for `readv` operation
///
/// The `readv` system call reads data into multiple buffers from a file descriptor.
/// It is equivalent to multiple `read` calls but is more efficient.
pub struct SysReadVHandle;

impl Syscall for SysReadVHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let iov = Self::iov(args);
        let count = Self::count(args);

        // IoVecs会进行用户态检验
        let iovecs = unsafe { IoVecs::from_user(iov, count, true) }?;

        let mut data = vec![0; iovecs.total_len()];

        let len = do_read(fd, &mut data)?;

        iovecs.scatter(&data[..len]);

        return Ok(len);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("iov", format!("{:#x}", Self::iov(args) as usize)),
            FormattedSyscallParam::new("count", Self::count(args).to_string()),
        ]
    }
}

impl SysReadVHandle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn iov(args: &[usize]) -> *const IoVec {
        args[1] as *const IoVec
    }

    fn count(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_READV, SysReadVHandle);
