//! Linux epoll_pwait2: timespec timeout and a temporary signal mask.
use super::epoll_utils::do_epoll_pwait;
use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_EPOLL_PWAIT2};
use crate::filesystem::epoll::event_poll::EventPoll;
use crate::mm::VirtAddr;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::read_one_from_user_protected;
use crate::time::PosixTimeSpec;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysEpollPwait2Handle;

impl Syscall for SysEpollPwait2Handle {
    fn num_args(&self) -> usize {
        6
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let deadline = if args[3] == 0 {
            None
        } else {
            // __kernel_timespec consists of two signed 64-bit fields.
            let mut timeout = PosixTimeSpec::default();
            unsafe { read_one_from_user_protected(VirtAddr::new(args[3]), &mut timeout)? };
            if !timeout.is_valid_timeout() {
                return Err(SystemError::EINVAL);
            }
            Some(EventPoll::timeout_to_deadline(timeout))
        };
        do_epoll_pwait(
            args[0] as i32,
            VirtAddr::new(args[1]),
            args[2] as i32,
            deadline,
            VirtAddr::new(args[4]),
            args[5],
        )
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        [
            "epfd",
            "events",
            "maxevents",
            "timeout",
            "sigmask",
            "sigsetsize",
        ]
        .iter()
        .zip(args.iter())
        .map(|(name, value)| FormattedSyscallParam::new(name, format!("{:#x}", value)))
        .collect()
    }
}

syscall_table_macros::declare_syscall!(SYS_EPOLL_PWAIT2, SysEpollPwait2Handle);
