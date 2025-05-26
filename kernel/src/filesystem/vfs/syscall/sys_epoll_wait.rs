//! System call handler for epoll_wait.

use crate::arch::syscall::nr::SYS_EPOLL_WAIT;
use crate::filesystem::epoll::event_poll::EventPoll;
use crate::filesystem::epoll::EPollEvent;
use crate::mm::VirtAddr;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferWriter;
use crate::time::PosixTimeSpec;
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

pub(super) fn do_epoll_wait(
    epfd: i32,
    events: VirtAddr,
    max_events: i32,
    timeout: i32,
) -> Result<usize, SystemError> {
    if max_events <= 0 || max_events as u32 > EventPoll::EP_MAX_EVENTS {
        return Err(SystemError::EINVAL);
    }

    let mut timespec = None;
    if timeout == 0 {
        timespec = Some(PosixTimeSpec::new(0, 0));
    }

    if timeout > 0 {
        let sec: i64 = timeout as i64 / 1000;
        let nsec: i64 = 1000000 * (timeout as i64 % 1000);

        timespec = Some(PosixTimeSpec::new(sec, nsec))
    }

    // 从用户传入的地址中拿到epoll_events
    let mut epds_writer = UserBufferWriter::new(
        events.as_ptr::<EPollEvent>(),
        max_events as usize * core::mem::size_of::<EPollEvent>(),
        true,
    )?;

    let epoll_events = epds_writer.buffer::<EPollEvent>(0)?;
    return EventPoll::epoll_wait(epfd, epoll_events, max_events, timespec);
}
