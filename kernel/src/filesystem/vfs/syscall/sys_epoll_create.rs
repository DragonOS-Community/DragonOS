//! System call handler for epoll creation.

use crate::arch::syscall::nr::SYS_EPOLL_CREATE;
use crate::filesystem::epoll::event_poll::EventPoll;
use crate::filesystem::vfs::file::FileMode;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysEpollCreateHandle;

impl Syscall for SysEpollCreateHandle {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let max_size = Self::max_size(args);
        if max_size < 0 {
            return Err(SystemError::EINVAL);
        }

        return EventPoll::create_epoll(FileMode::empty());
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "max_size",
            format!("{:#x}", Self::max_size(args) as usize),
        )]
    }
}

impl SysEpollCreateHandle {
    fn max_size(args: &[usize]) -> i32 {
        args[0] as i32
    }
}

syscall_table_macros::declare_syscall!(SYS_EPOLL_CREATE, SysEpollCreateHandle);
