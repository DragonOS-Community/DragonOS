use core::ffi::c_int;

use crate::{
    arch::ipc::signal::{SigSet, Signal},
    filesystem::epoll::{event_poll::EventPoll, EPollCtlOption, EPollEvent, EPollEventType},
    ipc::signal::{
        restore_saved_sigmask_unless, set_user_sigmask, RestartBlock, RestartBlockData, RestartFn,
    },
    libs::wait_queue::{TimeoutWaker, Waiter},
    mm::VirtAddr,
    process::ProcessManager,
    syscall::{
        user_access::{UserBufferReader, UserBufferWriter},
        Syscall,
    },
    time::{
        syscall::PosixTimeval,
        timer::{next_n_us_timer_jiffies, Timer},
        Duration, Instant, PosixTimeSpec,
    },
};

use super::vfs::file::{File, FileFlags};
use crate::process::resource::RLimitID;
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
    /// 记录已添加到 epoll 的 fd 以及合并后的事件掩码
    added_fds: alloc::collections::BTreeMap<i32, u16>,
}

impl<'a> PollAdapter<'a> {
    pub fn new(ep_file: Arc<File>, poll_fds: &'a mut [PollFd]) -> Self {
        Self {
            ep_file,
            poll_fds,
            added_fds: alloc::collections::BTreeMap::new(),
        }
    }

    fn add_pollfds(&mut self) -> Result<(), SystemError> {
        // 首先清除所有 revents（revents 是 output-only 字段）
        // 这确保每次 poll 调用都从干净状态开始，不受之前调用的影响
        for pollfd in self.poll_fds.iter_mut() {
            pollfd.revents = 0;
        }

        // 第一遍：收集每个唯一 fd 的合并事件掩码
        for i in 0..self.poll_fds.len() {
            let pollfd = &self.poll_fds[i];
            if pollfd.fd < 0 {
                continue;
            }

            // 合并同一 fd 的所有事件
            let entry = self.added_fds.entry(pollfd.fd).or_insert(0);
            *entry |= pollfd.events;
        }

        // 第二遍：将每个唯一 fd 添加到 epoll
        let fds_to_add: alloc::vec::Vec<_> = self
            .added_fds
            .iter()
            .map(|(&fd, &events)| (fd, events))
            .collect();

        for (fd, merged_events) in fds_to_add {
            let mut epoll_event = EPollEvent::default();
            let mut poll_flags = PollFlags::from_bits_truncate(merged_events);
            poll_flags |= PollFlags::POLLERR | PollFlags::POLLHUP;
            let ep_events: EPollEventType = poll_flags.into();
            epoll_event.set_events(ep_events.bits());
            epoll_event.set_data(fd as u64);

            let result = EventPoll::epoll_ctl_with_epfile(
                self.ep_file.clone(),
                EPollCtlOption::Add,
                fd,
                epoll_event,
                false,
            );

            match result {
                Ok(_) => {}
                Err(_) => {
                    // 根据 POSIX 语义，无效的 fd 应该设置 POLLNVAL 并继续处理其他 fd
                    // 从 added_fds 中移除此 fd，并设置对应的所有 pollfd 条目的 revents
                    // 注意：同一个 fd 可能在 poll_fds 数组中出现多次，每个条目都应独立处理
                    self.added_fds.remove(&fd);
                    for pollfd in self.poll_fds.iter_mut() {
                        if pollfd.fd == fd {
                            pollfd.revents = PollFlags::POLLNVAL.bits();
                            // 不能 break，需要为所有匹配的条目设置 POLLNVAL
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn poll_all_fds(&mut self, timeout: Option<Instant>) -> Result<usize, SystemError> {
        // 如果没有添加任何有效 fd 到 epoll，直接计算已有 revents 的数量并返回
        // 这处理了所有 fd 都无效（POLLNVAL）的情况
        if self.added_fds.is_empty() {
            let count = self.poll_fds.iter().filter(|pfd| pfd.revents != 0).count();
            return Ok(count);
        }

        // 检查是否已经有无效 fd（POLLNVAL）
        let has_invalid_fds = self.poll_fds.iter().any(|pfd| pfd.revents != 0);

        // 即使某些 fd 已经有 POLLNVAL，仍需检查有效 fd 的事件
        // Linux 语义：poll 检查所有 fd 并在单次调用中报告所有就绪事件
        let mut epoll_events = vec![EPollEvent::default(); self.added_fds.len()];
        let len = epoll_events.len() as i32;

        // 如果已经有无效 fd，使用非阻塞模式检查有效 fd
        // 这样可以立即返回所有结果，而不会因等待有效 fd 而延迟报告无效 fd
        let remain_timeout = if has_invalid_fds {
            Some(PosixTimeSpec::default()) // timeout=0，非阻塞
        } else {
            timeout.map(|t| {
                t.duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO)
                    .into()
            })
        };

        let events = EventPoll::epoll_wait_with_file(
            self.ep_file.clone(),
            &mut epoll_events,
            len,
            remain_timeout,
        )?;

        // 处理返回的事件，将它们映射回所有相关的 pollfd 条目
        for event in epoll_events.iter().take(events) {
            let event_fd = event.data() as i32;
            let revents = event.events();

            // 找到所有匹配这个 fd 的 pollfd 条目
            for pollfd in self.poll_fds.iter_mut() {
                if pollfd.fd == event_fd {
                    // 只设置用户请求的事件 + 强制事件
                    let requested = (pollfd.events as u32)
                        | PollFlags::POLLERR.bits() as u32
                        | PollFlags::POLLHUP.bits() as u32
                        | PollFlags::POLLNVAL.bits() as u32;
                    let filtered_revents = revents & requested;
                    if filtered_revents != 0 {
                        pollfd.revents = (filtered_revents & 0xffff) as u16;
                    }
                }
            }
        }

        // 计算有事件的 pollfd 数量
        let count = self.poll_fds.iter().filter(|pfd| pfd.revents != 0).count();
        Ok(count)
    }
}

impl Syscall {
    /// https://code.dragonos.org.cn/xref/linux-6.6.21/fs/select.c#1068
    #[inline(never)]
    pub fn poll(pollfd_ptr: usize, nfds: u32, timeout_ms: i32) -> Result<usize, SystemError> {
        // 检查 nfds 是否超过 RLIMIT_NOFILE
        let rlimit_nofile = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::Nofile)
            .rlim_cur as u32;
        if nfds > rlimit_nofile {
            return Err(SystemError::EINVAL);
        }

        // 检查长度溢出
        let len = (nfds as usize)
            .checked_mul(core::mem::size_of::<PollFd>())
            .ok_or(SystemError::EINVAL)?;

        // 当 nfds > 0 但 pollfd_ptr 为空指针时，返回 EFAULT
        if nfds > 0 && pollfd_ptr == 0 {
            return Err(SystemError::EFAULT);
        }

        let pollfd_ptr = VirtAddr::new(pollfd_ptr);

        let mut timeout: Option<Instant> = None;
        if timeout_ms >= 0 {
            timeout = poll_select_set_timeout(timeout_ms as u64);
        }

        // nfds == 0 时，直接进入等待逻辑，不需要用户缓冲区
        if nfds == 0 {
            let mut r = do_sys_poll(&mut [], timeout);
            if let Err(SystemError::ERESTARTNOHAND) = r {
                let restart_block_data = RestartBlockData::new_poll(pollfd_ptr, nfds, timeout);
                let restart_block = RestartBlock::new(&RestartFnPoll, restart_block_data);
                r = ProcessManager::current_pcb().set_restart_fn(Some(restart_block));
            }
            return r;
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
        // 检查 nfds 是否超过 RLIMIT_NOFILE
        let rlimit_nofile = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::Nofile)
            .rlim_cur as u32;
        if nfds > rlimit_nofile {
            return Err(SystemError::EINVAL);
        }

        // 检查长度溢出
        let pollfds_len = (nfds as usize)
            .checked_mul(core::mem::size_of::<PollFd>())
            .ok_or(SystemError::EINVAL)?;

        // 当 nfds > 0 但 pollfd_ptr 为空指针时，返回 EFAULT
        if nfds > 0 && pollfd_ptr == 0 {
            return Err(SystemError::EFAULT);
        }

        let mut timeout_ts: Option<Instant> = None;
        let mut sigmask: Option<SigSet> = None;
        let pollfd_ptr = VirtAddr::new(pollfd_ptr);

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

        // nfds == 0 时，直接进入等待逻辑，不需要用户缓冲区
        let mut r: Result<usize, SystemError> = if nfds == 0 {
            do_sys_poll(&mut [], timeout_ts)
        } else {
            let mut poll_fds_writer =
                UserBufferWriter::new(pollfd_ptr.as_ptr::<PollFd>(), pollfds_len, true)?;
            let poll_fds = poll_fds_writer.buffer(0)?;
            do_sys_poll(poll_fds, timeout_ts)
        };

        // 处理信号中断，设置restart block使ppoll可重启
        if let Err(SystemError::ERESTARTNOHAND) = r {
            let restart_block_data = RestartBlockData::new_poll(pollfd_ptr, nfds, timeout_ts);
            let restart_block = RestartBlock::new(&RestartFnPoll, restart_block_data);
            r = ProcessManager::current_pcb().set_restart_fn(Some(restart_block));
        }

        return poll_select_finish(timeout_ts, timespec_ptr, PollTimeType::TimeSpec, r);
    }
}

pub fn do_sys_poll(
    poll_fds: &mut [PollFd],
    timeout: Option<Instant>,
) -> Result<usize, SystemError> {
    // 特殊处理: nfds=0 时，直接进入可中断等待
    // 这种情况下只等待超时或信号，不需要创建 epoll
    if poll_fds.is_empty() {
        return poll_wait_timeout_only(timeout);
    }

    let ep_file = EventPoll::create_epoll_file(FileFlags::empty())?;

    let ep_file = Arc::new(ep_file);

    let mut adapter = PollAdapter::new(ep_file, poll_fds);
    adapter.add_pollfds()?;
    let nevents = adapter.poll_all_fds(timeout)?;

    Ok(nevents)
}

/// 处理 nfds=0 的情况：纯等待超时或信号
///
/// 根据 Linux 语义：
/// - 如果 timeout=0，立即返回 0
/// - 如果 timeout>0，等待指定时间后返回 0
/// - 如果 timeout=-1(None)，无限等待直到被信号中断，返回 ERESTARTNOHAND
fn poll_wait_timeout_only(timeout: Option<Instant>) -> Result<usize, SystemError> {
    // 如果有超时时间且已过期，直接返回
    if let Some(end_time) = timeout {
        if Instant::now() >= end_time {
            return Ok(0);
        }
    }

    loop {
        // 检查是否有待处理的信号
        let current_pcb = ProcessManager::current_pcb();
        if current_pcb.has_pending_signal_fast()
            && Signal::signal_pending_state(true, false, &current_pcb)
        {
            return Err(SystemError::ERESTARTNOHAND);
        }

        // 检查超时
        if let Some(end_time) = timeout {
            if Instant::now() >= end_time {
                return Ok(0);
            }
        }

        // 计算剩余等待时间
        let sleep_duration = if let Some(end_time) = timeout {
            let remain = end_time
                .duration_since(Instant::now())
                .unwrap_or(Duration::ZERO);
            if remain == Duration::ZERO {
                return Ok(0);
            }
            remain
        } else {
            // 无限等待时，设置一个较长的时间（比如1秒），然后循环检查信号
            Duration::from_secs(1)
        };

        // 创建 Waiter/Waker 对
        let (waiter, waker) = Waiter::new_pair();

        // 创建定时器唤醒
        let sleep_us = sleep_duration.total_micros();
        let timer: Arc<Timer> = Timer::new(
            TimeoutWaker::new(waker.clone()),
            next_n_us_timer_jiffies(sleep_us),
        );

        // 激活定时器
        timer.activate();

        // 使用标准的 Waiter.wait() - 它内部已经正确处理了中断禁用等逻辑
        let wait_res = waiter.wait(true);

        // 醒来后检查原因
        let was_timeout = timer.timeout();
        if !was_timeout {
            timer.cancel();
        }

        // 处理等待结果（可能是信号中断）
        // 注意：waiter.wait() 返回 ERESTARTSYS，但 poll 需要 ERESTARTNOHAND
        // 以便正确设置 restart block
        if let Err(SystemError::ERESTARTSYS) = wait_res {
            return Err(SystemError::ERESTARTNOHAND);
        }
        wait_res?;

        // 检查是否因信号而醒来
        let current_pcb = ProcessManager::current_pcb();
        if current_pcb.has_pending_signal_fast()
            && Signal::signal_pending_state(true, false, &current_pcb)
        {
            return Err(SystemError::ERESTARTNOHAND);
        }

        // 如果超时且有超时设置，返回 0
        if was_timeout && timeout.is_some() {
            return Ok(0);
        }

        // 无限等待时继续循环（重新检查信号）
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
