//! System call handler for epoll_pwait.

use crate::arch::ipc::signal::SigSet;
use crate::arch::syscall::nr::SYS_EPOLL_PWAIT;
use crate::ipc::signal::restore_saved_sigmask;
use crate::ipc::signal::set_user_sigmask;
use crate::mm::VirtAddr;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferReader;
use alloc::vec::Vec;
use system_error::SystemError;

use super::epoll_utils::do_epoll_wait;

pub struct SysEpollPwaitHandle;

impl Syscall for SysEpollPwaitHandle {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let epfd = Self::epfd(args);
        let epoll_event = Self::epoll_event(args);
        let max_events = Self::max_events(args);
        let timespec = Self::timespec(args);
        let sigmask_addr = Self::sigmask_addr(args);

        if sigmask_addr.is_null() {
            return do_epoll_wait(epfd, epoll_event, max_events, timespec);
        }
        let sigmask_reader =
            UserBufferReader::new(sigmask_addr, core::mem::size_of::<SigSet>(), true)?;
        let mut sigmask = *sigmask_reader.read_one_from_user::<SigSet>(0)?;

        set_user_sigmask(&mut sigmask);

        let wait_ret = do_epoll_wait(epfd, epoll_event, max_events, timespec);

        if wait_ret.is_err() && *wait_ret.as_ref().unwrap_err() != SystemError::EINTR {
            restore_saved_sigmask();
        }
        wait_ret
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("epfd", format!("{:#x}", Self::epfd(args) as usize)),
            FormattedSyscallParam::new("event", format!("{:#x}", Self::epoll_event(args).data())),
            FormattedSyscallParam::new("max_events", format!("{:#x}", Self::max_events(args))),
            FormattedSyscallParam::new("timespec", format!("{:#x}", Self::timespec(args))),
            FormattedSyscallParam::new(
                "sigmask_addr",
                format!("{:#x}", Self::sigmask_addr(args) as usize),
            ),
        ]
    }
}

impl SysEpollPwaitHandle {
    fn epfd(args: &[usize]) -> i32 {
        args[0] as i32
    }
    fn epoll_event(args: &[usize]) -> VirtAddr {
        VirtAddr::new(args[1])
    }
    fn max_events(args: &[usize]) -> i32 {
        args[2] as i32
    }
    fn timespec(args: &[usize]) -> i32 {
        args[3] as i32
    }
    fn sigmask_addr(args: &[usize]) -> *mut SigSet {
        args[4] as *mut SigSet
    }
}

syscall_table_macros::declare_syscall!(SYS_EPOLL_PWAIT, SysEpollPwaitHandle);
