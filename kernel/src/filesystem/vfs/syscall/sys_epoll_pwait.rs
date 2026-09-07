//! System call handler for epoll_pwait.

use super::epoll_utils::{do_epoll_pwait, epoll_msec_deadline};
use crate::arch::interrupt::TrapFrame;
use crate::arch::ipc::signal::SigSet;
use crate::arch::syscall::nr::SYS_EPOLL_PWAIT;
use crate::mm::VirtAddr;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysEpollPwaitHandle;

impl Syscall for SysEpollPwaitHandle {
    fn num_args(&self) -> usize {
        6
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let epfd = Self::epfd(args);
        let epoll_event = Self::epoll_event(args);
        let max_events = Self::max_events(args);
        let timeout = Self::timeout(args);
        let sigmask_addr = Self::sigmask_addr(args);

        do_epoll_pwait(
            epfd,
            epoll_event,
            max_events,
            epoll_msec_deadline(timeout),
            VirtAddr::new(sigmask_addr as usize),
            args[5],
        )
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("epfd", format!("{:#x}", Self::epfd(args) as usize)),
            FormattedSyscallParam::new("event", format!("{:#x}", Self::epoll_event(args).data())),
            FormattedSyscallParam::new("max_events", format!("{:#x}", Self::max_events(args))),
            FormattedSyscallParam::new("timeout", format!("{:#x}", Self::timeout(args))),
            FormattedSyscallParam::new(
                "sigmask_addr",
                format!("{:#x}", Self::sigmask_addr(args) as usize),
            ),
            FormattedSyscallParam::new("sigsetsize", format!("{}", args[5])),
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
    fn timeout(args: &[usize]) -> i32 {
        args[3] as i32
    }
    fn sigmask_addr(args: &[usize]) -> *mut SigSet {
        args[4] as *mut SigSet
    }
}

syscall_table_macros::declare_syscall!(SYS_EPOLL_PWAIT, SysEpollPwaitHandle);
