//! System call handler for epoll_ctl.

use crate::arch::syscall::nr::SYS_EPOLL_CTL;
use crate::filesystem::epoll::event_poll::EventPoll;
use crate::filesystem::epoll::EPollCtlOption;
use crate::filesystem::epoll::EPollEvent;
use crate::mm::VirtAddr;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferReader;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysEpollCtlHandle;

impl Syscall for SysEpollCtlHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let op = EPollCtlOption::from_op_num(Self::op(args))?;
        let mut epds = EPollEvent::default();
        let event = Self::event(args);
        let epfd = Self::epfd(args);
        let fd = Self::fd(args);

        if op != EPollCtlOption::Del {
            // 不为EpollCtlDel时不允许传入空指针
            if event.is_null() {
                return Err(SystemError::EFAULT);
            }

            // 还是一样的问题，C标准的epoll_event大小为12字节，而内核实现的epoll_event内存对齐后为16字节
            // 这样分别拷贝其实和整体拷贝差别不大，内核使用内存对其版本甚至可能提升性能
            let epds_reader = UserBufferReader::new(
                event.as_ptr::<EPollEvent>(),
                core::mem::size_of::<EPollEvent>(),
                true,
            )?;

            // 拷贝到内核
            epds_reader.copy_one_from_user(&mut epds, 0)?;
        }

        return EventPoll::epoll_ctl_with_epfd(epfd, op, fd, epds, false);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("epfd", format!("{:#x}", Self::epfd(args) as usize)),
            FormattedSyscallParam::new("op", format!("{:#x}", Self::op(args))),
            FormattedSyscallParam::new("fd", format!("{:#x}", Self::fd(args) as usize)),
            FormattedSyscallParam::new("event", format!("{:#x}", Self::event(args).data())),
        ]
    }
}

impl SysEpollCtlHandle {
    fn epfd(args: &[usize]) -> i32 {
        args[0] as i32
    }
    fn op(args: &[usize]) -> usize {
        args[1]
    }
    fn fd(args: &[usize]) -> i32 {
        args[2] as i32
    }
    fn event(args: &[usize]) -> VirtAddr {
        VirtAddr::new(args[3])
    }
}

syscall_table_macros::declare_syscall!(SYS_EPOLL_CTL, SysEpollCtlHandle);
