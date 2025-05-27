//! System call handler for epoll_create1.

use crate::arch::syscall::nr::SYS_EPOLL_CREATE1;
use crate::filesystem::epoll::event_poll::EventPoll;
use crate::filesystem::vfs::file::FileMode;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysEpollCreate1Handle;

impl Syscall for SysEpollCreate1Handle {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        return EventPoll::create_epoll(Self::flags(args));
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "flags",
            format!("{:#x}", Self::flags(args)),
        )]
    }
}

impl SysEpollCreate1Handle {
    fn flags(args: &[usize]) -> FileMode {
        FileMode::from_bits_truncate(args[0] as u32)
    }
}

syscall_table_macros::declare_syscall!(SYS_EPOLL_CREATE1, SysEpollCreate1Handle);
