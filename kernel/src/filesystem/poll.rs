use core::ffi::c_int;

use crate::{
    arch::ipc::signal::SigSet,
    ipc::signal::{
        restore_saved_sigmask_unless, set_user_sigmask, RestartBlock, RestartBlockData, RestartFn,
    },
    mm::VirtAddr,
    net::event_poll::{EPollCtlOption, EPollEvent, EPollEventType, EventPoll},
    process::ProcessManager,
    syscall::{
        user_access::{UserBufferReader, UserBufferWriter},
        Syscall,
    },
    time::{Duration, Instant, PosixTimeSpec},
};

use super::vfs::file::{File, FileMode};
use alloc::sync::Arc;
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
}

impl<'a> PollAdapter<'a> {
    pub fn new(ep_file: Arc<File>, poll_fds: &'a mut [PollFd]) -> Self {
        Self { ep_file, poll_fds }
    }

    fn add_pollfds(&self) -> Result<(), SystemError> {
        for (i, pollfd) in self.poll_fds.iter().enumerate() {
            if pollfd.fd < 0 {
                continue;
            }
            let mut epoll_event = EPollEvent::default();
            let poll_flags = PollFlags::from_bits_truncate(pollfd.events);
            let ep_events: EPollEventType = poll_flags.into();
            epoll_event.set_events(ep_events.bits());
            epoll_event.set_data(i as u64);

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
        let mut epoll_events = vec![EPollEvent::default(); self.poll_fds.len()];
        let len = epoll_events.len() as i32;
        let remain_timeout = timeout
            .and_then(|t| t.duration_since(Instant::now()))
            .map(|t| t.into());
        let events = EventPoll::epoll_wait_with_file(
            self.ep_file.clone(),
            &mut epoll_events,
            len,
            remain_timeout,
        )?;

        for event in epoll_events.iter() {
            let index = event.data() as usize;
            if index >= self.poll_fds.len() {
                log::warn!("poll_all_fds: Invalid index in epoll event: {}", index);
                continue;
            }
            self.poll_fds[index].revents = (event.events() & 0xffff) as u16;
        }

        Ok(events)
    }
}

impl Syscall {
    /// https://code.dragonos.org.cn/xref/linux-6.6.21/fs/select.c#1068
    #[inline(never)]
    pub fn poll(pollfd_ptr: usize, nfds: u32, timeout_ms: i32) -> Result<usize, SystemError> {
        let pollfd_ptr = VirtAddr::new(pollfd_ptr);
        let len = nfds as usize * core::mem::size_of::<PollFd>();

        let mut timeout: Option<Instant> = None;
        if timeout_ms >= 0 {
            timeout = poll_select_set_timeout(timeout_ms as u64);
        }
        let mut poll_fds_writer = UserBufferWriter::new(pollfd_ptr.as_ptr::<PollFd>(), len, true)?;
        let mut r = do_sys_poll(poll_fds_writer.buffer(0)?, timeout);
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
        let mut timeout_ts: Option<Instant> = None;
        let mut sigmask: Option<SigSet> = None;
        let pollfd_ptr = VirtAddr::new(pollfd_ptr);
        let pollfds_len = nfds as usize * core::mem::size_of::<PollFd>();
        let mut poll_fds_writer =
            UserBufferWriter::new(pollfd_ptr.as_ptr::<PollFd>(), pollfds_len, true)?;
        let poll_fds = poll_fds_writer.buffer(0)?;
        if sigmask_ptr != 0 {
            let sigmask_reader =
                UserBufferReader::new(sigmask_ptr as *const SigSet, size_of::<SigSet>(), true)?;
            sigmask = Some(*sigmask_reader.read_one_from_user(0)?);
        }

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

        if let Some(mut sigmask) = sigmask {
            set_user_sigmask(&mut sigmask);
        }
        // log::debug!(
        //     "ppoll: poll_fds: {:?}, nfds: {}, timeout_ts: {:?}，sigmask: {:?}",
        //     poll_fds,
        //     nfds,
        //     timeout_ts,
        //     sigmask
        // );

        let r: Result<usize, SystemError> = do_sys_poll(poll_fds, timeout_ts);

        return poll_select_finish(timeout_ts, timespec_ptr, PollTimeType::TimeSpec, r);
    }
}

fn do_sys_poll(poll_fds: &mut [PollFd], timeout: Option<Instant>) -> Result<usize, SystemError> {
    let ep_file = EventPoll::create_epoll_file(FileMode::empty())?;

    let ep_file = Arc::new(ep_file);

    let mut adapter = PollAdapter::new(ep_file, poll_fds);
    adapter.add_pollfds()?;
    let nevents = adapter.poll_all_fds(timeout)?;

    Ok(nevents)
}

/// 计算超时的时刻
fn poll_select_set_timeout(timeout_ms: u64) -> Option<Instant> {
    if timeout_ms == 0 {
        return None;
    }

    Some(Instant::now() + Duration::from_millis(timeout_ms))
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/select.c#298
fn poll_select_finish(
    end_time: Option<Instant>,
    user_time_ptr: usize,
    poll_time_type: PollTimeType,
    mut result: Result<usize, SystemError>,
) -> Result<usize, SystemError> {
    restore_saved_sigmask_unless(result == Err(SystemError::ERESTARTNOHAND));

    if user_time_ptr == 0 {
        return result;
    }

    // todo: 处理sticky timeouts

    if end_time.is_none() {
        return result;
    }

    let end_time = end_time.unwrap();

    // no update for zero timeout
    if end_time.total_millis() <= 0 {
        return result;
    }

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
            if tswriter.copy_one_to_user(&rts, 0).is_err() {
                return result;
            }
        }
        _ => todo!(),
    }

    if result == Err(SystemError::ERESTARTNOHAND) {
        result = result.map_err(|_| SystemError::EINTR);
    }

    return result;
}

#[allow(unused)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollTimeType {
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
