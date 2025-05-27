//! System call handler for epoll_wait.

use super::epoll_utils::do_epoll_wait;
use crate::arch::syscall::nr::SYS_EPOLL_WAIT;
use crate::mm::VirtAddr;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysEpollWaitHandle;

impl Syscall for SysEpollWaitHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let epfd = Self::epfd(args);
        let max_events = Self::max_events(args);
        let timeout = Self::timeout(args);
        let events = Self::events(args);

        do_epoll_wait(epfd, events, max_events, timeout)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("epfd", format!("{:#x}", Self::epfd(args) as usize)),
            FormattedSyscallParam::new("events", format!("{:#x}", Self::events(args).data())),
            FormattedSyscallParam::new("max_events", format!("{:#x}", Self::max_events(args))),
            FormattedSyscallParam::new("timeout", format!("{:#x}", Self::timeout(args))),
        ]
    }
}

impl SysEpollWaitHandle {
    fn epfd(args: &[usize]) -> i32 {
        args[0] as i32
    }
    fn events(args: &[usize]) -> VirtAddr {
        VirtAddr::new(args[1])
    }
    fn max_events(args: &[usize]) -> i32 {
        args[2] as i32
    }
    fn timeout(args: &[usize]) -> i32 {
        args[3] as i32
    }
}

syscall_table_macros::declare_syscall!(SYS_EPOLL_WAIT, SysEpollWaitHandle);
