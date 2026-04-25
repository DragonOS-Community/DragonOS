use core::ffi::c_int;

use crate::{
    filesystem::epoll::{event_poll::EventPoll, EPollCtlOption, EPollEvent, EPollEventType},
    ipc::signal::{restore_saved_sigmask_unless, RestartBlock, RestartBlockData, RestartFn},
    libs::wait_queue::{TimeoutWaker, Waiter},
    process::ProcessManager,
    syscall::user_access::UserBufferWriter,
    syscall::user_buffer::UserBuffer,
    time::{
        syscall::PosixTimeval,
        timer::{next_n_us_timer_jiffies, Timer},
        Duration, Instant, PosixTimeSpec,
    },
};

use super::vfs::file::{File, FileFlags};
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PollFd {
    pub fd: c_int,
    pub events: u16,
    pub revents: u16,
}

#[inline]
fn pollfd_revents_offset() -> usize {
    let base = core::ptr::null::<PollFd>();
    unsafe { core::ptr::addr_of!((*base).revents) as usize - base as usize }
}

pub(crate) fn read_pollfds_from_user(
    user_buf: &mut UserBuffer<'_>,
    nfds: usize,
) -> Result<Vec<PollFd>, SystemError> {
    let elem_size = core::mem::size_of::<PollFd>();
    let total_len = nfds.checked_mul(elem_size).ok_or(SystemError::EINVAL)?;
    if user_buf.len() < total_len {
        return Err(SystemError::EFAULT);
    }

    let mut poll_fds = vec![
        PollFd {
            fd: 0,
            events: 0,
            revents: 0
        };
        nfds
    ];
    let dst_bytes =
        unsafe { core::slice::from_raw_parts_mut(poll_fds.as_mut_ptr() as *mut u8, total_len) };
    let copied = user_buf.read_from_user(0, dst_bytes)?;
    if copied != total_len {
        return Err(SystemError::EFAULT);
    }
    Ok(poll_fds)
}

pub(crate) fn write_pollfds_revents_to_user(
    user_buf: &mut UserBuffer<'_>,
    poll_fds: &[PollFd],
) -> Result<(), SystemError> {
    let elem_size = core::mem::size_of::<PollFd>();
    let total_len = poll_fds
        .len()
        .checked_mul(elem_size)
        .ok_or(SystemError::EINVAL)?;
    if user_buf.len() < total_len {
        return Err(SystemError::EFAULT);
    }
    let revents_off = pollfd_revents_offset();
    for (i, pollfd) in poll_fds.iter().enumerate() {
        let off = i
            .checked_mul(elem_size)
            .and_then(|v| v.checked_add(revents_off))
            .ok_or(SystemError::EINVAL)?;
        let bytes = pollfd.revents.to_ne_bytes();
        let written = user_buf.write_to_user(off, &bytes)?;
        if written != bytes.len() {
            return Err(SystemError::EFAULT);
        }
    }
    Ok(())
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

        // Linux 语义：poll(2) 允许普通文件/目录参与 poll，并立即返回就绪。
        // 但 epoll(7) 明确不允许监听普通文件/目录（返回 EPERM）。
        // 我们的 poll 实现内部复用了 epoll，因此必须在这里对"总是就绪"类型做特判，
        // 避免把 EPERM 误当成无效 fd(POLLNVAL) 并进一步导致 select 返回 EBADF。
        //
        // 注意：即使某些 inode 实现了 PollableInode（supports_poll()=true），
        // 普通文件/目录在 poll(2) 里仍然应当表现为"不会阻塞且总是就绪"，
        // 因此这里不能以 supports_poll() 作为排除条件。

        for (fd, merged_events) in fds_to_add {
            // 先判断 fd 是否存在，以及是否属于“总是就绪”类型。
            let dst_file = {
                let current_pcb = ProcessManager::current_pcb();
                let fd_table = current_pcb.fd_table();
                let fd_table_guard = fd_table.read();
                fd_table_guard.get_file_by_fd(fd)
            };

            match dst_file {
                None => {
                    // 无效 fd：设置 POLLNVAL
                    self.added_fds.remove(&fd);
                    for pollfd in self.poll_fds.iter_mut() {
                        if pollfd.fd == fd {
                            pollfd.revents = PollFlags::POLLNVAL.bits();
                        }
                    }
                    continue;
                }
                Some(file) if file.is_always_ready() => {
                    // 普通文件/目录：立即就绪，revents = events & DEFAULT_POLLMASK。
                    self.added_fds.remove(&fd);
                    for pollfd in self.poll_fds.iter_mut() {
                        if pollfd.fd == fd {
                            pollfd.revents = pollfd.events & PollFlags::DEFAULT_POLLMASK.bits();
                        }
                    }
                    continue;
                }
                Some(_) => {}
            }

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

            if result.is_err() {
                // 对于无法加入 epoll 的情况，按“无效/不可监听”处理：设置 POLLNVAL。
                // “总是就绪”文件已经在上面特判并被移除。
                self.added_fds.remove(&fd);
                for pollfd in self.poll_fds.iter_mut() {
                    if pollfd.fd == fd {
                        pollfd.revents = PollFlags::POLLNVAL.bits();
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
        // 检查是否有未被掩码屏蔽的待处理信号
        // ppoll 应该只被未被屏蔽的信号中断，被屏蔽的信号应保持 pending 状态
        let current_pcb = ProcessManager::current_pcb();
        if current_pcb.has_pending_signal_fast() && current_pcb.has_pending_not_masked_signal() {
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

        // 检查是否因未被掩码屏蔽的信号而醒来
        let current_pcb = ProcessManager::current_pcb();
        if current_pcb.has_pending_signal_fast() && current_pcb.has_pending_not_masked_signal() {
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

    // 将ERESTARTSYS、ERESTARTNOHAND和ERESTART_RESTARTBLOCK转换为EINTR
    // 这些错误码表示系统调用被信号中断，应该返回EINTR给用户态
    if matches!(
        result,
        Err(SystemError::ERESTARTNOHAND)
            | Err(SystemError::ERESTARTSYS)
            | Err(SystemError::ERESTART_RESTARTBLOCK)
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

        /// 默认的 poll 掩码，用于不支持 poll 的普通文件/目录
        ///
        /// 对应 Linux 内核的 DEFAULT_POLLMASK (POLLIN | POLLOUT | POLLRDNORM | POLLWRNORM)
        const DEFAULT_POLLMASK = Self::POLLIN.bits() | Self::POLLOUT.bits()
            | Self::POLLRDNORM.bits() | Self::POLLWRNORM.bits();
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
pub struct RestartFnPoll;

impl RestartFn for RestartFnPoll {
    // 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/fs/select.c#1047
    fn call(&self, data: &mut RestartBlockData) -> Result<usize, SystemError> {
        if let RestartBlockData::Poll(d) = data {
            let len = d.nfds as usize * core::mem::size_of::<PollFd>();

            let mut poll_fds_writer =
                UserBufferWriter::new(d.pollfd_ptr.as_ptr::<PollFd>(), len, true)?;
            let mut user_buf = poll_fds_writer.buffer_protected(0)?;
            let mut poll_fds = read_pollfds_from_user(&mut user_buf, d.nfds as usize)?;
            let mut r = do_sys_poll(&mut poll_fds, d.timeout_instant);
            if let Err(e) = write_pollfds_revents_to_user(&mut user_buf, &poll_fds) {
                r = Err(e);
            }
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
