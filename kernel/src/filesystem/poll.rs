use core::ffi::c_int;

use crate::{
    arch::ipc::signal::SigSet,
    filesystem::epoll::{event_poll::EventPoll, EPollCtlOption, EPollEvent, EPollEventType},
    ipc::signal::{
        restore_saved_sigmask_unless, set_user_sigmask, RestartBlock, RestartBlockData, RestartFn,
    },
    libs::wait_queue::TimeoutWaiter,
    mm::VirtAddr,
    process::{resource::RLimitID, ProcessManager},
    syscall::{
        user_access::{UserBufferReader, UserBufferWriter},
        Syscall,
    },
    time::{syscall::PosixTimeval, Duration, Instant, PosixTimeSpec},
};

use super::vfs::file::{File, FileFlags};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use system_error::SystemError;

#[repr(C)]
#[derive(Debug)]
pub struct PollFd {
    pub fd: c_int,
    pub events: u16,
    pub revents: u16,
}

struct PollAdapter<'a> {
    ep_file: Arc<File>,
    poll_fds: &'a mut [PollFd],
    /// Maps fd to all indices in poll_fds array that reference this fd
    fd_to_indices: BTreeMap<i32, Vec<usize>>,
}

impl<'a> PollAdapter<'a> {
    pub fn new(ep_file: Arc<File>, poll_fds: &'a mut [PollFd]) -> Self {
        Self {
            ep_file,
            poll_fds,
            fd_to_indices: BTreeMap::new(),
        }
    }

    fn add_pollfds(&mut self) -> Result<(), SystemError> {
        for (i, pollfd) in self.poll_fds.iter().enumerate() {
            if pollfd.fd < 0 {
                continue;
            }

            // Track all indices for this fd
            let indices = self.fd_to_indices.entry(pollfd.fd).or_default();
            let is_first = indices.is_empty();
            indices.push(i);

            // Only add to epoll if this is the first occurrence of this fd
            if !is_first {
                continue;
            }

            let mut epoll_event = EPollEvent::default();
            let poll_flags = PollFlags::from_bits_truncate(pollfd.events);
            let mut ep_events: EPollEventType = poll_flags.into();
            // POLLERR and POLLHUP are always reported, regardless of what events were requested
            // This matches Linux's behavior: filter = demangle_poll(pollfd->events) | EPOLLERR | EPOLLHUP;
            ep_events |= EPollEventType::EPOLLERR | EPollEventType::EPOLLHUP;
            epoll_event.set_events(ep_events.bits());
            // Store the fd as data so we can look up all indices later
            epoll_event.set_data(pollfd.fd as u64);

            EventPoll::epoll_ctl_with_epfile(
                self.ep_file.clone(),
                EPollCtlOption::Add,
                pollfd.fd,
                epoll_event,
                false,
            )
            .map(|_| ())?;
        }

        Ok(())
    }

    fn poll_all_fds(&mut self, timeout: Option<Instant>) -> Result<usize, SystemError> {
        // Number of unique fds (not total poll_fds)
        let unique_fds = self.fd_to_indices.len();
        let mut epoll_events = vec![EPollEvent::default(); unique_fds];
        let len = epoll_events.len() as i32;
        let remain_timeout = timeout.map(|t| {
            t.duration_since(Instant::now())
                .unwrap_or(Duration::ZERO)
                .into()
        });
        let events = EventPoll::epoll_wait_with_file(
            self.ep_file.clone(),
            &mut epoll_events,
            len,
            remain_timeout,
        )?;

        let mut total_ready = 0usize;

        for event in epoll_events.iter().take(events) {
            // data contains the fd
            let fd = event.data() as i32;
            let actual_events = event.events();

            // Get all indices for this fd
            let indices = match self.fd_to_indices.get(&fd) {
                Some(indices) => indices,
                None => continue,
            };

            // Update all poll_fds entries for this fd
            for &index in indices {
                if index >= self.poll_fds.len() {
                    continue;
                }

                // Apply the correct filter for this specific entry
                let requested = self.poll_fds[index].events as u32;
                let filter =
                    requested | EPollEventType::EPOLLERR.bits() | EPollEventType::EPOLLHUP.bits();
                let filtered_events = actual_events & filter;

                if filtered_events != 0 {
                    self.poll_fds[index].revents = (filtered_events & 0xffff) as u16;
                    total_ready += 1;
                }
            }
        }
        Ok(total_ready)
    }
}

impl Syscall {
    /// https://code.dragonos.org.cn/xref/linux-6.6.21/fs/select.c#1068
    #[inline(never)]
    pub fn poll(pollfd_ptr: usize, nfds: u32, timeout_ms: i32) -> Result<usize, SystemError> {
        // Check nfds against RLIMIT_NOFILE first (like Linux does)
        // This catches negative nfds values that become large positive values when cast to u32
        let nofile_limit = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::Nofile)
            .rlim_cur as usize;
        if nfds as usize > nofile_limit {
            return Err(SystemError::EINVAL);
        }

        let mut timeout: Option<Instant> = None;
        if timeout_ms >= 0 {
            timeout = poll_select_set_timeout(timeout_ms as u64);
        }

        // Handle nfds=0 as a special case - no buffer needed
        // This handles poll(nullptr, 0, timeout) correctly
        if nfds == 0 {
            let mut r = poll_no_fds(timeout);
            if let Err(SystemError::ERESTARTNOHAND) = r {
                let pollfd_ptr = VirtAddr::new(pollfd_ptr);
                let restart_block_data = RestartBlockData::new_poll(pollfd_ptr, nfds, timeout);
                let restart_block = RestartBlock::new(&RestartFnPoll, restart_block_data);
                r = ProcessManager::current_pcb().set_restart_fn(Some(restart_block));
            }
            return r;
        }

        let pollfd_ptr = VirtAddr::new(pollfd_ptr);
        let len = nfds as usize * core::mem::size_of::<PollFd>();

        let mut poll_fds_writer = UserBufferWriter::new(pollfd_ptr.as_ptr::<PollFd>(), len, true)?;
        let poll_fds = poll_fds_writer.buffer(0)?;

        let mut r = do_sys_poll(poll_fds, timeout);

        if let Err(SystemError::ERESTARTNOHAND) = r {
            let restart_block_data = RestartBlockData::new_poll(pollfd_ptr, nfds, timeout);
            let restart_block = RestartBlock::new(&RestartFnPoll, restart_block_data);
            r = ProcessManager::current_pcb().set_restart_fn(Some(restart_block));
        }

        return r;
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/select.c#1101
    #[inline(never)]
    pub fn ppoll(
        pollfd_ptr: usize,
        nfds: u32,
        timespec_ptr: usize,
        sigmask_ptr: usize,
    ) -> Result<usize, SystemError> {
        // Check nfds against RLIMIT_NOFILE first (like Linux does)
        let nofile_limit = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::Nofile)
            .rlim_cur as usize;
        if nfds as usize > nofile_limit {
            return Err(SystemError::EINVAL);
        }

        let mut timeout_ts: Option<Instant> = None;
        let mut sigmask: Option<SigSet> = None;

        // Read sigmask first (before any potential blocking)
        if sigmask_ptr != 0 {
            let sigmask_reader =
                UserBufferReader::new(sigmask_ptr as *const SigSet, size_of::<SigSet>(), true)?;
            sigmask = Some(*sigmask_reader.read_one_from_user(0)?);
        }

        // Read timeout
        if timespec_ptr != 0 {
            let tsreader = UserBufferReader::new(
                timespec_ptr as *const PosixTimeSpec,
                size_of::<PosixTimeSpec>(),
                true,
            )?;
            let ts: PosixTimeSpec = *tsreader.read_one_from_user(0)?;
            let timeout_ms = ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000;

            if timeout_ms >= 0 {
                timeout_ts =
                    Some(poll_select_set_timeout(timeout_ms as u64).ok_or(SystemError::EINVAL)?);
            }
        }

        // Set sigmask before blocking
        if let Some(mut sigmask) = sigmask {
            set_user_sigmask(&mut sigmask);
        }

        // Handle nfds=0 as a special case - no buffer needed
        if nfds == 0 {
            let r: Result<usize, SystemError> = poll_no_fds(timeout_ts);
            return poll_select_finish(timeout_ts, timespec_ptr, PollTimeType::TimeSpec, r);
        }

        let pollfd_ptr = VirtAddr::new(pollfd_ptr);
        let pollfds_len = nfds as usize * core::mem::size_of::<PollFd>();
        let mut poll_fds_writer =
            UserBufferWriter::new(pollfd_ptr.as_ptr::<PollFd>(), pollfds_len, true)?;
        let poll_fds = poll_fds_writer.buffer(0)?;

        let r: Result<usize, SystemError> = do_sys_poll(poll_fds, timeout_ts);

        return poll_select_finish(timeout_ts, timespec_ptr, PollTimeType::TimeSpec, r);
    }
}

pub fn do_sys_poll(
    poll_fds: &mut [PollFd],
    timeout: Option<Instant>,
) -> Result<usize, SystemError> {
    // Handle the case of no file descriptors to poll
    // This is a special case where we just need to wait for signals or timeout
    if poll_fds.is_empty() {
        return poll_no_fds(timeout);
    }

    let ep_file = EventPoll::create_epoll_file(FileFlags::empty())?;

    let ep_file = Arc::new(ep_file);

    let mut adapter = PollAdapter::new(ep_file, poll_fds);
    adapter.add_pollfds()?;
    let nevents = adapter.poll_all_fds(timeout)?;

    Ok(nevents)
}

/// Handle poll with no file descriptors - wait for signals or timeout
fn poll_no_fds(timeout: Option<Instant>) -> Result<usize, SystemError> {
    let current_pcb = ProcessManager::current_pcb();

    // Check for pending signals first
    if current_pcb.has_pending_signal_fast() && current_pcb.has_pending_not_masked_signal() {
        return Err(SystemError::ERESTARTNOHAND);
    }

    // Handle immediate timeout (timeout == Some(instant_in_past) or timeout == Some(now))
    if let Some(end_time) = timeout {
        if Instant::now() >= end_time {
            return Ok(0);
        }
    }

    // Calculate remaining timeout in microseconds
    let timeout_us = timeout.map(|end_time| {
        let now = Instant::now();
        if now >= end_time {
            0u64
        } else {
            end_time
                .duration_since(now)
                .unwrap_or(Duration::ZERO)
                .micros()
        }
    });

    // If zero timeout, return immediately
    if timeout_us == Some(0) {
        return Ok(0);
    }

    // Use safe TimeoutWaiter pattern
    let timeout_waiter = TimeoutWaiter::new(timeout_us);

    loop {
        // Check for signals before waiting
        if current_pcb.has_pending_signal_fast() && current_pcb.has_pending_not_masked_signal() {
            return Err(SystemError::ERESTARTNOHAND);
        }

        // Wait with timeout using the safe Waiter/Waker pattern
        match timeout_waiter.wait(true) {
            Ok(true) => return Ok(0), // Timed out
            Ok(false) => {
                // Woken up - check if by signal
                if current_pcb.has_pending_signal_fast()
                    && current_pcb.has_pending_not_masked_signal()
                {
                    return Err(SystemError::ERESTARTNOHAND);
                }
                // For infinite timeout (None), continue waiting
                if timeout.is_none() {
                    continue;
                }
                // For finite timeout, check if expired
                if let Some(end_time) = timeout {
                    if Instant::now() >= end_time {
                        return Ok(0);
                    }
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// 计算超时的时刻
pub fn poll_select_set_timeout(timeout_ms: u64) -> Option<Instant> {
    Some(Instant::now() + Duration::from_millis(timeout_ms))
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/select.c#298
pub fn poll_select_finish(
    end_time: Option<Instant>,
    user_time_ptr: usize,
    poll_time_type: PollTimeType,
    mut result: Result<usize, SystemError>,
) -> Result<usize, SystemError> {
    // 如果系统调用被信号中断（ERESTARTNOHAND或ERESTARTSYS），不恢复信号掩码
    // 因为信号处理函数可能需要使用新的信号掩码
    restore_saved_sigmask_unless(matches!(
        result,
        Err(SystemError::ERESTARTNOHAND) | Err(SystemError::ERESTARTSYS)
    ));

    if user_time_ptr == 0 {
        return result;
    }

    // todo: 处理sticky timeouts

    if end_time.is_none() {
        return result;
    }

    let end_time = end_time.unwrap();

    let ts = Instant::now();
    let duration = end_time.saturating_sub(ts);
    let rts: PosixTimeSpec = duration.into();

    match poll_time_type {
        PollTimeType::TimeSpec => {
            let mut tswriter = UserBufferWriter::new(
                user_time_ptr as *mut PosixTimeSpec,
                size_of::<PosixTimeSpec>(),
                true,
            )?;
            if tswriter.buffer_protected(0)?.write_one(0, &rts).is_err() {
                return result;
            }
        }
        PollTimeType::TimeVal | PollTimeType::OldTimeVal => {
            let rtv = PosixTimeval {
                tv_sec: rts.tv_sec,
                tv_usec: (rts.tv_nsec / 1000) as i32,
            };
            let mut tvwriter = UserBufferWriter::new(
                user_time_ptr as *mut PosixTimeval,
                size_of::<PosixTimeval>(),
                true,
            )?;
            if tvwriter.buffer_protected(0)?.write_one(0, &rtv).is_err() {
                return result;
            }
        }
        PollTimeType::OldTimeSpec => {
            // OldTimeSpec使用与TimeSpec相同的处理方式
            let mut tswriter = UserBufferWriter::new(
                user_time_ptr as *mut PosixTimeSpec,
                size_of::<PosixTimeSpec>(),
                true,
            )?;
            if tswriter.buffer_protected(0)?.write_one(0, &rts).is_err() {
                return result;
            }
        }
    }

    // 将ERESTARTSYS和ERESTARTNOHAND转换为EINTR
    // 这些错误码表示系统调用被信号中断，应该返回EINTR给用户态
    if matches!(
        result,
        Err(SystemError::ERESTARTNOHAND) | Err(SystemError::ERESTARTSYS)
    ) {
        result = result.map_err(|_| SystemError::EINTR);
    }

    return result;
}

#[allow(unused)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollTimeType {
    TimeVal,
    OldTimeVal,
    TimeSpec,
    OldTimeSpec,
}

bitflags! {
    pub struct PollFlags: u16 {
        const POLLIN = 0x0001;
        const POLLPRI = 0x0002;
        const POLLOUT = 0x0004;
        const POLLERR = 0x0008;
        const POLLHUP = 0x0010;
        const POLLNVAL = 0x0020;
        const POLLRDNORM = 0x0040;
        const POLLRDBAND = 0x0080;
        const POLLWRNORM = 0x0100;
        const POLLWRBAND = 0x0200;
        const POLLMSG = 0x0400;
        const POLLREMOVE = 0x1000;
        const POLLRDHUP = 0x2000;
        const POLLFREE = 0x4000;
        const POLL_BUSY_LOOP = 0x8000;
    }
}

impl From<PollFlags> for EPollEventType {
    fn from(val: PollFlags) -> Self {
        let mut epoll_flags = EPollEventType::empty();

        if val.contains(PollFlags::POLLIN) {
            epoll_flags |= EPollEventType::EPOLLIN;
        }
        if val.contains(PollFlags::POLLPRI) {
            epoll_flags |= EPollEventType::EPOLLPRI;
        }
        if val.contains(PollFlags::POLLOUT) {
            epoll_flags |= EPollEventType::EPOLLOUT;
        }
        if val.contains(PollFlags::POLLERR) {
            epoll_flags |= EPollEventType::EPOLLERR;
        }
        if val.contains(PollFlags::POLLHUP) {
            epoll_flags |= EPollEventType::EPOLLHUP;
        }
        if val.contains(PollFlags::POLLNVAL) {
            epoll_flags |= EPollEventType::EPOLLNVAL;
        }
        if val.contains(PollFlags::POLLRDNORM) {
            epoll_flags |= EPollEventType::EPOLLRDNORM;
        }
        if val.contains(PollFlags::POLLRDBAND) {
            epoll_flags |= EPollEventType::EPOLLRDBAND;
        }
        if val.contains(PollFlags::POLLWRNORM) {
            epoll_flags |= EPollEventType::EPOLLWRNORM;
        }
        if val.contains(PollFlags::POLLWRBAND) {
            epoll_flags |= EPollEventType::EPOLLWRBAND;
        }
        if val.contains(PollFlags::POLLMSG) {
            epoll_flags |= EPollEventType::EPOLLMSG;
        }
        if val.contains(PollFlags::POLLRDHUP) {
            epoll_flags |= EPollEventType::EPOLLRDHUP;
        }
        if val.contains(PollFlags::POLLFREE) {
            epoll_flags |= EPollEventType::POLLFREE;
        }

        epoll_flags
    }
}

/// sys_poll的restart fn
#[derive(Debug)]
struct RestartFnPoll;

impl RestartFn for RestartFnPoll {
    // 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/fs/select.c#1047
    fn call(&self, data: &mut RestartBlockData) -> Result<usize, SystemError> {
        if let RestartBlockData::Poll(d) = data {
            let len = d.nfds as usize * core::mem::size_of::<PollFd>();

            let mut poll_fds_writer =
                UserBufferWriter::new(d.pollfd_ptr.as_ptr::<PollFd>(), len, true)?;
            let mut r = do_sys_poll(poll_fds_writer.buffer(0)?, d.timeout_instant);
            if let Err(SystemError::ERESTARTNOHAND) = r {
                let restart_block = RestartBlock::new(&RestartFnPoll, data.clone());
                r = ProcessManager::current_pcb().set_restart_fn(Some(restart_block));
            }

            return r;
        } else {
            panic!("RestartFnPoll called with wrong data type: {:?}", data);
        }
    }
}
