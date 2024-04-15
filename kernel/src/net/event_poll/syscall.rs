use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::ipc::signal::SigSet, filesystem::vfs::file::FileMode, ipc::signal::set_current_sig_blocked, mm::VirtAddr, process::ProcessManager, syscall::{
        user_access::{UserBufferReader, UserBufferWriter},
        Syscall,
    },
    time::PosixTimeSpec,
};

use super::{EPollCtlOption, EPollEvent, EventPoll, Pollfd};

impl Syscall {
    pub fn epoll_create(max_size: i32) -> Result<usize, SystemError> {
        if max_size < 0 {
            return Err(SystemError::EINVAL);
        }

        return EventPoll::do_create_epoll(FileMode::empty());
    }

    pub fn epoll_create1(flag: usize) -> Result<usize, SystemError> {
        let flags = FileMode::from_bits_truncate(flag as u32);

        let ret = EventPoll::do_create_epoll(flags);
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
        return EventPoll::do_epoll_wait(epfd, epoll_events, max_events, timespec);
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

        return EventPoll::do_epoll_ctl(epfd, op, fd, &mut epds, false);
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
        set_current_sig_blocked(sigmask);

        let wait_ret = Self::epoll_wait(epfd, epoll_event, max_events, timespec);

        if wait_ret.is_err() && *wait_ret.as_ref().unwrap_err() != SystemError::EINTR {
            // TODO: 恢复信号?
            // link：https://code.dragonos.org.cn/xref/linux-6.1.9/fs/eventpoll.c#2294
        }
        wait_ret
    }

    pub fn poll(ufds: VirtAddr,nfds: u32,timeout_msecs: i32) -> Result<usize,SystemError>{


        let fds = {
            let mut read_add = ufds;
            let mut fds:Vec<Pollfd> = Vec::with_capacity(nfds as usize);
            for _ in 0..nfds{
                let reader = UserBufferReader::new(
                    read_add.as_ptr::<Pollfd>(),
                    core::mem::size_of::<Pollfd>(),
                    true
                )?;
                let mut fd = Pollfd::default();
                reader.copy_one_from_user::<Pollfd>(&mut fd, 0)?;
                fds.push(fd);
                read_add += core::mem::size_of::<Pollfd>();
            }
            fds
        };

        let mut timespec = None;
        if timeout_msecs>=0 {
            let sec = timeout_msecs as i64 / 1000;
            let nsec = 1000000 *(timeout_msecs as i64 % 1000);
            timespec = Some(TimeSpec::new(sec,nsec));
        }
        let nums_events = Self::do_poll(&fds,timespec)?;


        let mut write_add = ufds;
        
        for fd in fds {
            let mut writer = UserBufferWriter::new(
            write_add.as_ptr::<Pollfd>(), 
            core::mem::size_of::<Pollfd>(), 
            false)?;
            writer.copy_one_to_user(&fd, 0)?;
            write_add += core::mem::size_of::<Pollfd>();
        }
        
        Ok(nums_events)
    }
    pub fn do_poll(fds: &[Pollfd],timeout: Option<TimeSpec>) -> Result<usize,SystemError> {
        loop{
            let mut revent_nums = 0;
            for fd in fds{
                let binding = ProcessManager::current_pcb().fd_table();
                let fd_table_guard = binding.read();
                let file = fd_table_guard
                    .get_file_by_fd(fd.fd)
                    .ok_or(SystemError::EBADF)?.clone();
                drop(fd_table_guard);
                file.lock_irqsave().poll();
            }

            if(!timeout.is_none()&&timeout.unwrap().tv_sec==0&&timeout.unwrap().tv_nsec==0){
                return Ok(0);
            }
        }
    }
}
