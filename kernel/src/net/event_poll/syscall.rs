use system_error::SystemError;

use crate::{
    arch::ipc::signal::SigSet,
    filesystem::vfs::file::FileMode,
    ipc::signal::{restore_saved_sigmask, set_user_sigmask},
    mm::VirtAddr,
    syscall::{
        user_access::{UserBufferReader, UserBufferWriter},
        Syscall,
    },
    time::PosixTimeSpec,
};

use super::{EPollCtlOption, EPollEvent, EventPoll};

impl Syscall {
    pub fn epoll_create(max_size: i32) -> Result<usize, SystemError> {
        if max_size < 0 {
            return Err(SystemError::EINVAL);
        }

        return EventPoll::create_epoll(FileMode::empty());
    }

    pub fn epoll_create1(flag: usize) -> Result<usize, SystemError> {
        let flags = FileMode::from_bits_truncate(flag as u32);

        let ret = EventPoll::create_epoll(flags);
        ret
    }

    pub fn epoll_wait(
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

    pub fn epoll_ctl(epfd: i32, op: usize, fd: i32, event: VirtAddr) -> Result<usize, SystemError> {
        let op = EPollCtlOption::from_op_num(op)?;
        let mut epds = EPollEvent::default();
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

    /// ## 在epoll_wait时屏蔽某些信号
    pub fn epoll_pwait(
        epfd: i32,
        epoll_event: VirtAddr,
        max_events: i32,
        timespec: i32,
        sigmask: &mut SigSet,
    ) -> Result<usize, SystemError> {
        // 设置屏蔽的信号
        set_user_sigmask(sigmask);

        let wait_ret = Self::epoll_wait(epfd, epoll_event, max_events, timespec);

        if wait_ret.is_err() && *wait_ret.as_ref().unwrap_err() != SystemError::EINTR {
            restore_saved_sigmask();
        }
        wait_ret
    }
}
