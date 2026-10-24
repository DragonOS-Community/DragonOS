use crate::{
    filesystem::vfs::{
        file::{File, FileFlags},
        FilePrivateData,
    },
    libs::{
        mutex::{Mutex, MutexGuard},
        prepared_queue::Queue,
        rbtree::RBTree,
        spinlock::SpinLock,
        wait_queue::{TimeoutWaker, WaitQueue, Waiter},
    },
    process::ProcessManager,
    time::{
        timekeeping::monotonic_now,
        timer::{next_n_us_timer_jiffies, Timer},
        PosixTimeSpec,
    },
};
use core::fmt::Debug;

use alloc::{
    collections::BTreeSet,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use super::{fs::EPollInode, EPollCtlOption, EPollEvent, EPollEventType, EPollItem};

use super::epitem::{ReadyNode, ReadyQueue, Registration};

/// Linux ep->lock boundary. Ready/overflow nodes are charged and prepared
/// at registration birth; callbacks neither allocate nor acquire ep->mtx.
pub(crate) struct ReadyState {
    ready_list: ReadyQueue,
    ovflist: Option<ReadyQueue>,
    epoll_wq: WaitQueue,
}

impl ReadyState {
    /// The owning ready lock also serializes membership and spare ownership.
    fn enqueue(&mut self, item: &Arc<EPollItem>) -> bool {
        let mut links = item.links.lock_irqsave();
        let overflow = self.ovflist.is_some();
        let queued = if overflow {
            &mut links.overflow
        } else {
            &mut links.ready
        };
        if *queued {
            return false;
        }
        *queued = true;
        let mut node = links
            .spare
            .pop_node()
            .expect("at most one ready and one overflow node");
        node.value_mut().item = Arc::downgrade(item);
        match self.ovflist.as_mut() {
            Some(queue) => queue.push_node(node),
            None => self.ready_list.push_node(node),
        }
        true
    }
}

fn retire_ready_queue(mut queue: ReadyQueue) {
    while let Some(node) = queue.pop_node() {
        drop(node.into_value());
    }
}

impl Drop for ReadyState {
    fn drop(&mut self) {
        retire_ready_queue(core::mem::take(&mut self.ready_list));
        if let Some(queue) = self.ovflist.take() {
            retire_ready_queue(queue);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct EPollKey {
    open_file_id: usize,
    fd: i32,
}

impl EPollKey {
    pub(super) fn new(file: &File, fd: i32) -> Self {
        Self {
            open_file_id: file.open_file_id(),
            fd,
        }
    }
}

impl Debug for ReadyState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadyState")
            .field("ready_list_len", &self.ready_list.len())
            .field("ovflist_active", &self.ovflist.is_some())
            .finish()
    }
}

/// 内核的Epoll对象结构体，当用户创建一个Epoll时，内核就会创建一个该类型对象
/// 它对应一个epfd
///
/// ## 双锁设计（对标 Linux 6.6 `struct eventpoll`）
///
/// - **外层 Mutex**（`LockedEventPoll` / `Arc<Mutex<EventPoll>>`）：保护 `ep_items`
///   红黑树及结构性修改。由 `epoll_ctl`、`ep_send_events` 扫描阶段持有。可睡眠。
///
/// - **内层 SpinLock**（`ready_state`）：保护 `ready_list`、`ovflist`、`epoll_wq`。
///   使用 `lock_irqsave`，hardirq 安全。由回调路径 `wakeup_epoll` 以及
///   `ep_start_scan`/`ep_done_scan` 短暂持有。
#[derive(Debug)]
pub struct EventPoll {
    /// 维护所有添加进来的socket的红黑树（由外层 Mutex 保护）
    ep_items: RBTree<EPollKey, Arc<EPollItem>>,
    /// 就绪状态（由内层 irqsave SpinLock 保护）
    ready_state: Arc<SpinLock<ReadyState>>,
    /// 监听本 epollfd 的 epitems（用于支持 epoll 嵌套：epollfd 被加入另一个 epoll）
    pub(super) poll_epitems: Arc<EPollItemList>,
    self_ref: Option<Weak<Mutex<EventPoll>>>,
}

impl EventPoll {
    pub const EP_MAX_EVENTS: u32 = u32::MAX / (core::mem::size_of::<EPollEvent>() as u32);
    /// 用于获取inode中的epitem队列
    pub const ADD_EPOLLITEM: u32 = 0x7965;

    fn new() -> Self {
        Self {
            ep_items: RBTree::new(),
            ready_state: Arc::new(SpinLock::new(ReadyState {
                ready_list: Queue::new(),
                ovflist: None,
                epoll_wq: WaitQueue::default(),
            })),
            poll_epitems: Arc::new(EPollItemList::default()),
            self_ref: None,
        }
    }

    /// 关闭epoll时，执行的逻辑
    pub(super) fn close(&mut self) -> Result<(), SystemError> {
        let keys: Vec<EPollKey> = self.ep_items.keys().copied().collect();
        for key in keys {
            let Some(epitem) = self.ep_items.get(&key).cloned() else {
                continue;
            };
            if let Some(file) = epitem.file().upgrade() {
                let _ = file.remove_epitem(&epitem);
            }
            Self::deactivate_and_remove_ready(self, &epitem);
            self.ep_items.remove(&key);
        }

        Ok(())
    }

    /// ## 创建epoll对象, 并将其加入到当前进程的fd_table中
    ///
    /// ### 参数
    /// - flags: 创建的epoll文件的FileMode
    ///
    /// ### 返回值
    /// - 成功则返回Ok(fd)，否则返回Err
    pub fn create_epoll(flags: FileFlags) -> Result<usize, SystemError> {
        let ep_file = Self::create_epoll_file(flags)?;
        let cloexec = flags.contains(FileFlags::O_CLOEXEC);

        let current_pcb = ProcessManager::current_pcb();
        let fd_table = current_pcb.fd_table();
        let ep_file = Arc::try_new(ep_file).map_err(|_| SystemError::ENOMEM)?;
        let fd = File::recover_io(|| {
            fd_table.alloc_fd_arc(
                ep_file.clone(),
                cloexec,
                current_pcb.nofile_soft_limit(),
                &crate::cgroup::memory::object_for_task(&current_pcb),
            )
        })?;

        Ok(fd as usize)
    }

    /// ## 创建epoll文件
    pub fn create_epoll_file(flags: FileFlags) -> Result<File, SystemError> {
        if !flags.difference(FileFlags::O_CLOEXEC).is_empty() {
            return Err(SystemError::EINVAL);
        }

        // 创建epoll
        let storage = crate::filesystem::vfs::file::PreparedFile::new_recovering()?;
        let epoll = Self::do_create_epoll();

        // 创建epoll的inode对象
        let epoll_inode = EPollInode::new(epoll.clone());

        let mut ep_file = File::new_prepared_recovering(
            epoll_inode,
            FileFlags::O_RDWR | (flags & FileFlags::O_CLOEXEC),
            storage,
        )?;

        // 设置ep_file的FilePrivateData
        ep_file.private_data = Mutex::new(FilePrivateData::EPoll(EPollPrivateData { epoll }));
        Ok(ep_file)
    }

    fn do_create_epoll() -> LockedEventPoll {
        let epoll = LockedEventPoll(Arc::new(Mutex::new(EventPoll::new())));
        epoll.0.lock().self_ref = Some(Arc::downgrade(&epoll.0));
        epoll
    }

    /// ## epoll_ctl的具体实现
    ///
    /// 根据不同的op对epoll文件进行增删改
    ///
    /// ### 参数
    /// - ep_file: epoll文件
    /// - op: 对应的操作
    /// - dstfd: 操作对应的文件描述符
    /// - dst_file: 操作对应的文件(与dstfd对应)
    /// - epds: 从用户态传入的event，若op为EpollCtlAdd，则对应注册的监听事件，若op为EPollCtlMod，则对应更新的事件，删除操作不涉及此字段
    /// - nonblock: 定义这次操作是否为非阻塞（有可能其他地方占有EPoll的锁）
    fn do_epoll_ctl(
        ep_file: Arc<File>,
        op: EPollCtlOption,
        dstfd: i32,
        dst_file: Arc<File>,
        mut epds: EPollEvent,
        nonblock: bool,
    ) -> Result<usize, SystemError> {
        // 检查是否允许 EPOLLWAKEUP
        if op != EPollCtlOption::Del {
            epds.events &= !EPollEventType::EPOLLWAKEUP.bits();
        }

        // Linux 语义：EPOLLERR/EPOLLHUP 会被无条件报告。
        // 由于 EPollItem::ep_item_poll 会与 interested mask 相交，这里需要把它们强制加入 mask。
        if op != EPollCtlOption::Del {
            epds.events |= EPollEventType::EPOLLERR.bits() | EPollEventType::EPOLLHUP.bits();
        }

        let events = EPollEventType::from_bits_truncate(epds.events);

        // 检查获取到的两个文件的正确性
        // 首先是不能自己嵌套自己
        // 然后ep_file必须是epoll文件
        if Arc::ptr_eq(&ep_file, &dst_file) || !Self::is_epoll_file(&ep_file) {
            return Err(SystemError::EINVAL);
        }

        if op != EPollCtlOption::Del && events.contains(EPollEventType::EPOLLEXCLUSIVE) {
            // epoll独占模式下不允许EpollCtlMod
            if op == EPollCtlOption::Mod {
                return Err(SystemError::EINVAL);
            }

            // 不支持嵌套的独占唤醒
            if op == EPollCtlOption::Add && Self::is_epoll_file(&dst_file)
                || epds.events & !EPollEventType::EPOLLEXCLUSIVE_OK_BITS.bits() != 0
            {
                return Err(SystemError::EINVAL);
            }
        }

        // 从 FilePrivateData 获取到 epoll（注意：不能持有 private_data 锁跨越 loop check，
        // 否则在遍历到自身 epoll 文件时会二次加锁导致自旋死锁）。
        let epoll_data = {
            let guard = ep_file.private_data.lock();
            match &*guard {
                FilePrivateData::EPoll(d) => d.clone(),
                _ => return Err(SystemError::EINVAL),
            }
        };

        // 支持 epoll 嵌套，但必须进行循环/深度检查（Linux: ELOOP）。
        // 该检查不应在持有 src epoll 自旋锁时进行。
        if op == EPollCtlOption::Add && Self::is_epoll_file(&dst_file) {
            let dst_epoll = match &*dst_file.private_data.lock() {
                FilePrivateData::EPoll(d) => d.epoll.clone(),
                _ => return Err(SystemError::EINVAL),
            };
            let src_epoll = epoll_data.epoll.clone();
            Self::ep_loop_check(&src_epoll, &dst_epoll)?;
        }

        let key = EPollKey::new(&dst_file, dstfd);
        let candidate = if op == EPollCtlOption::Add {
            // Linux validates the target's poll operation before epitem
            // allocation. Do not replace these errors with quota ENOMEM.
            if dst_file.is_always_ready() {
                return Err(SystemError::EPERM);
            }
            if !dst_file.supports_poll() {
                return Err(SystemError::ENOSYS);
            }
            let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::new();
            loop {
                let (ready_state, poll_epitems) = {
                    let guard = if nonblock {
                        epoll_data
                            .epoll
                            .0
                            .try_lock()
                            .map_err(|_| SystemError::EAGAIN_OR_EWOULDBLOCK)?
                    } else {
                        epoll_data.epoll.0.lock()
                    };
                    // Existing registrations must fail before birth/charge.
                    if guard.ep_items.get(&key).is_some() {
                        return Err(SystemError::EEXIST);
                    }
                    (
                        Arc::downgrade(&guard.ready_state),
                        Arc::downgrade(&guard.poll_epitems),
                    )
                };
                let creator =
                    crate::cgroup::memory::object_for_task(&ProcessManager::current_pcb());
                match EPollItem::try_new(
                    &creator,
                    Arc::downgrade(&epoll_data.epoll.0),
                    ready_state,
                    poll_epitems,
                    key,
                    epds,
                    Arc::downgrade(&dst_file),
                ) {
                    Ok(item) => break Some(item),
                    // The failed entity and all its receipts already dropped;
                    // this is outside source/private/IRQ resource guards.
                    Err(error) => error.recover(&mut recovery)?,
                }
            }
        } else {
            None
        };
        // Pins outlive the commit guard on every error/rollback path. In
        // particular DEL must not free an accounted body under the mutex.
        let mut retired = None;
        let notify_nested = {
            let mut epoll_guard = {
                if nonblock {
                    // 如果设置非阻塞，则尝试获取一次锁
                    if let Ok(guard) = epoll_data.epoll.0.try_lock() {
                        guard
                    } else {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }
                } else {
                    epoll_data.epoll.0.lock()
                }
            };

            let ep_item = epoll_guard.ep_items.get(&key).cloned();
            let notify_nested = match op {
                EPollCtlOption::Add => {
                    // 如果已经存在，则返回错误
                    if ep_item.is_some() {
                        return Err(SystemError::EEXIST);
                    }

                    Self::ep_insert(
                        &mut epoll_guard,
                        dst_file.clone(),
                        candidate
                            .as_ref()
                            .expect("ADD has prepared candidate")
                            .clone(),
                    )?
                }
                EPollCtlOption::Del => match ep_item {
                    Some(ref ep_item) => {
                        retired = Some(ep_item.clone());
                        // 删除
                        Self::ep_remove(&mut epoll_guard, key, Some(dst_file.clone()), ep_item)?;
                        false
                    }
                    None => {
                        // 不存在则返回错误
                        return Err(SystemError::ENOENT);
                    }
                },
                EPollCtlOption::Mod => {
                    // 不存在则返回错误
                    if ep_item.is_none() {
                        return Err(SystemError::ENOENT);
                    }
                    let ep_item = ep_item.unwrap().clone();

                    // Linux rejects MOD of an existing exclusive registration,
                    // even when the new request omits EPOLLEXCLUSIVE.
                    if (ep_item.event.lock_irqsave().events & EPollEventType::EPOLLEXCLUSIVE.bits())
                        != 0
                    {
                        return Err(SystemError::EINVAL);
                    }

                    Self::ep_modify(&mut epoll_guard, ep_item, &epds)?
                }
            };

            if notify_nested {
                Some(epoll_guard.poll_epitems.clone())
            } else {
                None
            }
        };
        drop(retired);
        drop(candidate);

        if let Some(poll_epitems) = notify_nested {
            let _ = Self::wakeup_epoll(
                poll_epitems.as_ref(),
                EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
            );
        }

        Ok(0)
    }

    pub fn epoll_ctl_with_epfd(
        epfd: i32,
        op: EPollCtlOption,
        dstfd: i32,
        epds: EPollEvent,
        nonblock: bool,
    ) -> Result<usize, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        let fd_table = current_pcb.fd_table();
        let fd_table_guard = fd_table.read();

        // 获取epoll和对应fd指向的文件
        let ep_file = fd_table_guard
            .get_file_by_fd(epfd)
            .ok_or(SystemError::EBADF)?;
        let dst_file = fd_table_guard
            .get_file_by_fd(dstfd)
            .ok_or(SystemError::EBADF)?;

        drop(fd_table_guard);

        Self::do_epoll_ctl(ep_file, op, dstfd, dst_file, epds, nonblock)
    }

    pub fn epoll_ctl_with_epfile(
        ep_file: Arc<File>,
        op: EPollCtlOption,
        dstfd: i32,
        epds: EPollEvent,
        nonblock: bool,
    ) -> Result<usize, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        let fd_table = current_pcb.fd_table();
        let fd_table_guard = fd_table.read();
        let dst_file = fd_table_guard
            .get_file_by_fd(dstfd)
            .ok_or(SystemError::EBADF)?;

        drop(fd_table_guard);

        Self::do_epoll_ctl(ep_file, op, dstfd, dst_file, epds, nonblock)
    }

    pub fn epoll_wait(
        epfd: i32,
        max_events: i32,
        deadline: Option<PosixTimeSpec>,
        output: &mut dyn FnMut(usize, &EPollEvent) -> Result<(), SystemError>,
    ) -> Result<usize, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        let fd_table = current_pcb.fd_table();
        let fd_table_guard = fd_table.read();

        // 获取epoll文件
        let ep_file = fd_table_guard
            .get_file_by_fd(epfd)
            .ok_or(SystemError::EBADF)?;

        drop(fd_table_guard);
        Self::wait_with_output(ep_file, max_events, deadline, output)
    }

    /// ## epoll_wait的具体实现
    pub fn epoll_wait_with_file(
        ep_file: Arc<File>,
        epoll_event: &mut [EPollEvent],
        max_events: i32,
        timespec: Option<PosixTimeSpec>,
    ) -> Result<usize, SystemError> {
        if max_events <= 0 || epoll_event.len() < max_events as usize {
            return Err(SystemError::EINVAL);
        }
        Self::wait_with_output(
            ep_file,
            max_events,
            timespec.map(Self::timeout_to_deadline),
            &mut |index, event| {
                epoll_event[index] = *event;
                Ok(())
            },
        )
    }

    /// Convert a valid relative timeout once; zero remains the polling sentinel.
    pub(crate) fn timeout_to_deadline(timeout: PosixTimeSpec) -> PosixTimeSpec {
        if timeout.is_empty() {
            timeout
        } else {
            monotonic_now().saturating_add_ktime(&timeout)
        }
    }

    /// Shared wait engine. The deadline is absolute CLOCK_MONOTONIC.
    fn wait_with_output(
        ep_file: Arc<File>,
        max_events: i32,
        deadline: Option<PosixTimeSpec>,
        output: &mut dyn FnMut(usize, &EPollEvent) -> Result<(), SystemError>,
    ) -> Result<usize, SystemError> {
        let current_pcb = ProcessManager::current_pcb();

        // 确保是epoll file
        if !Self::is_epoll_file(&ep_file) {
            return Err(SystemError::EINVAL);
        }

        // 从epoll文件获取到epoll
        let mut epolldata = None;
        if let FilePrivateData::EPoll(epoll_data) = &*ep_file.private_data.lock() {
            epolldata = Some(epoll_data.clone())
        }
        if let Some(epoll_data) = epolldata {
            let epoll = epoll_data.epoll.clone();

            // 获取 ready_state 的 Arc，用于后续不需要外层 Mutex 的操作
            let rs_arc = {
                let ep_guard = epoll.0.lock();
                ep_guard.ready_state.clone()
            };

            let mut timeout = deadline.is_some_and(|time| time.is_empty());
            // 判断epoll上有没有就绪事件（仅需 SpinLock）
            let mut available = Self::ep_events_available_rs(&rs_arc);

            loop {
                if available {
                    // 如果有就绪的事件，则直接返回就绪事件
                    let sent = Self::ep_send_events(epoll.clone(), output, max_events)?;

                    // Linux 语义：阻塞等待时，被唤醒但没有可返回事件应继续等待。
                    // 这会发生在并发读/状态变化导致 ready_list 中项目在 poll 时已不再就绪。
                    if sent != 0 {
                        return Ok(sent);
                    }
                    if timeout {
                        return Ok(0);
                    }
                    available = false;
                    continue;
                }

                // 如果超时
                if timeout {
                    return Ok(0);
                }

                // 如果有未处理且未被屏蔽的信号则返回错误
                if current_pcb.has_pending_signal_fast()
                    && current_pcb.has_pending_not_masked_signal()
                {
                    // Linux epoll_wait(2): interrupted by signal handler -> EINTR.
                    // Returning ERESTARTSYS would cause userspace to restart the syscall
                    // (SA_RESTART), which breaks gVisor's UnblockWithSignal expectation.
                    return Err(SystemError::EINTR);
                }

                if deadline.is_some_and(|time| monotonic_now().to_ktime_ns() >= time.to_ktime_ns())
                {
                    return Ok(0);
                }

                // 自旋等待一段时间（仅需 SpinLock）
                available = {
                    let mut ret = false;
                    for _ in 0..50 {
                        if Self::ep_events_available_rs(&rs_arc) {
                            ret = true;
                            break;
                        }
                    }
                    if !ret {
                        ret = Self::ep_events_available_rs(&rs_arc);
                    }
                    ret
                };

                if available {
                    continue;
                }

                // 还未等待到事件发生，则睡眠
                // 构造一次等待（先构造 Waiter/Waker，超时需要通过 Waker::wake 触发）
                let (waiter, waker) = Waiter::new_pair();

                // 注册定时器：用 waker.wake() 来触发 waiter 退出等待（而不是仅唤醒 PCB）
                let mut timer = None;
                if let Some(deadline) = deadline {
                    let remain_ns = deadline
                        .to_ktime_ns()
                        .saturating_sub(monotonic_now().to_ktime_ns());
                    if remain_ns == 0 {
                        timeout = true;
                    } else {
                        let jiffies = next_n_us_timer_jiffies(remain_ns.div_ceil(1_000));
                        let inner: Arc<Timer> =
                            Timer::new(TimeoutWaker::new(waker.clone()), jiffies);
                        timer = Some(inner);
                    }
                }

                if timeout {
                    return Ok(0);
                }
                {
                    // 注册前再次检查，避免错过事件（仅需 SpinLock）
                    let rs = rs_arc.lock_irqsave();
                    if Self::ready_state_has_events(&rs) {
                        available = true;
                        // 不注册，直接继续
                    } else {
                        rs.epoll_wq.register_waker(waker.clone())?;
                    }
                }

                if available {
                    if let Some(timer) = timer {
                        timer.cancel();
                    }
                    continue;
                }

                if let Some(ref t) = timer {
                    t.activate();
                }

                let wait_res = match waiter.wait(true) {
                    Err(SystemError::ERESTARTSYS) => Err(SystemError::EINTR),
                    other => other,
                };

                {
                    // 清理 waker（仅需 SpinLock）
                    let rs = rs_arc.lock_irqsave();
                    rs.epoll_wq.remove_waker(&waker);
                    available = Self::ready_state_has_events(&rs);
                }

                if let Some(timer) = timer {
                    // A coarse timer firing alone does not establish expiry.
                    timeout = timer.timeout()
                        && deadline.is_some_and(|time| {
                            monotonic_now().to_ktime_ns() >= time.to_ktime_ns()
                        });
                    timer.cancel();
                }

                wait_res?;
            }
        } else {
            panic!("An epoll file does not have the corresponding private information");
        }
    }

    /// 通过 ready_state Arc 检查是否有就绪事件（不需要外层 Mutex）
    fn ep_events_available_rs(rs_arc: &Arc<SpinLock<ReadyState>>) -> bool {
        let rs = rs_arc.lock_irqsave();
        Self::ready_state_has_events(&rs)
    }

    #[inline]
    fn ready_state_has_events(rs: &ReadyState) -> bool {
        !rs.ready_list.is_empty() || rs.ovflist.is_some()
    }

    /// ep_start_scan: transfer existing nodes without allocation.
    fn ep_start_scan(&self) -> ReadyQueue {
        let mut rs = self.ready_state.lock_irqsave();
        let stolen = core::mem::take(&mut rs.ready_list);
        rs.ovflist = Some(Queue::new());
        stolen
    }

    /// ep_done_scan: overflow callbacks survive, but duplicate nodes return to
    /// the registration. The caller holds ep->mtx, so each live registration
    /// is pinned by ep_items throughout the short ready-state critical section.
    fn ep_done_scan(&self, mut remaining: ReadyQueue, mut delivered_lt: ReadyQueue) {
        let mut retired = Queue::new();
        {
            let mut rs = self.ready_state.lock_irqsave();
            let mut overflow = rs.ovflist.take().expect("matching ep_start_scan");
            while let Some(node) = overflow.pop_node() {
                let Some(item) = node.value().item.upgrade() else {
                    retired.push_node(node);
                    continue;
                };
                let mut links = item.links.lock_irqsave();
                links.overflow = false;
                if links.ready {
                    links.spare.push_node(node);
                } else {
                    links.ready = true;
                    rs.ready_list.push_node(node);
                }
            }
            // Linux ep_done_scan puts unprocessed txlist first, concurrent
            // overflow next, and already-delivered LT entries last. Continuous
            // callbacks must not starve the unprocessed tail at maxevents=1.
            rs.ready_list.append(&mut delivered_lt);
            remaining.append(&mut rs.ready_list);
            rs.ready_list = remaining;
            if !rs.ready_list.is_empty() {
                rs.epoll_wq.wakeup_all(None);
            }
        }
        retire_ready_queue(retired);
    }

    fn recycle_scanned(&self, item: &Arc<EPollItem>, node: ReadyNode) {
        let _rs = self.ready_state.lock_irqsave();
        let mut links = item.links.lock_irqsave();
        links.ready = false;
        links.spare.push_node(node);
    }

    fn ep_send_events(
        epoll: LockedEventPoll,
        output: &mut dyn FnMut(usize, &EPollEvent) -> Result<(), SystemError>,
        max_events: i32,
    ) -> Result<usize, SystemError> {
        let ep_guard = epoll.0.lock();
        let mut res = 0;
        let mut pending = ep_guard.ep_start_scan();
        let mut delivered_lt = Queue::new();
        let mut error = None;
        while res < max_events as usize {
            let Some(node) = pending.pop_node() else {
                break;
            };
            let Some(epitem) = node.value().item.upgrade() else {
                drop(node.into_value());
                continue;
            };
            let revents = epitem.ep_item_poll();
            if revents.is_empty() {
                ep_guard.recycle_scanned(&epitem, node);
                continue;
            }
            let registered = EPollEventType::from_bits_truncate(epitem.event.lock_irqsave().events);
            let is_oneshot = registered.contains(EPollEventType::EPOLLONESHOT);
            let is_edge = registered.contains(EPollEventType::EPOLLET);
            let event = EPollEvent {
                events: revents.bits,
                data: epitem.event.lock_irqsave().data,
            };
            if let Err(err) = output(res, &event) {
                // Fault must not consume ET/ONESHOT or rotate this entry.
                pending.push_front_node(node);
                error = Some(err);
                break;
            }
            res += 1;
            if is_oneshot {
                let mut event_guard = epitem.event.lock_irqsave();
                let new_event = event_guard.events & EPollEventType::EP_PRIVATE_BITS.bits;
                event_guard.set_events(new_event);
            }
            if is_oneshot || is_edge {
                ep_guard.recycle_scanned(&epitem, node);
            } else {
                delivered_lt.push_node(node);
            }
        }
        ep_guard.ep_done_scan(pending, delivered_lt);
        if res == 0 {
            if let Some(err) = error {
                return Err(err);
            }
        }
        Ok(res)
    }

    // ### 查看文件是否为epoll文件
    fn is_epoll_file(file: &Arc<File>) -> bool {
        if let FilePrivateData::EPoll(_) = *file.private_data.lock() {
            return true;
        }
        return false;
    }

    fn ep_insert(
        epoll_guard: &mut MutexGuard<EventPoll>,
        dst_file: Arc<File>,
        epitem: Arc<EPollItem>,
    ) -> Result<bool, SystemError> {
        // 检查文件是否为"总是就绪"类型（不支持poll的普通文件/目录）
        let is_always_ready = dst_file.is_always_ready();

        // Linux 语义：epoll 不允许监听普通文件与目录（这些对象的 I/O 不会阻塞且通常不实现 poll），返回 EPERM。
        // 参考: epoll(7) / gVisor epoll_test::RegularFiles
        if is_always_ready {
            return Err(SystemError::EPERM);
        }

        // 不支持poll的非普通文件，返回错误
        if !dst_file.supports_poll() && !is_always_ready {
            return Err(SystemError::ENOSYS);
        }

        // EPOLLWAKEUP 用于电源管理，暂时不支持
        if epitem.event.lock_irqsave().events & EPollEventType::EPOLLWAKEUP.bits() != 0 {
            return Err(SystemError::ENOSYS);
        }

        epoll_guard.ep_items.insert(epitem.key(), epitem.clone());

        // 先将 epitem 添加到目标文件的 epoll_items 中，这样之后的 notify/wakeup_epoll
        // 才能找到并唤醒这个 epitem。
        if let Err(e) = dst_file.add_epitem(epitem.clone()) {
            // 如果添加失败，需要清理 ep_items 中已插入的项
            epoll_guard.ep_items.remove(&epitem.key());
            return Err(e);
        }

        // 现在检查文件是否已经有事件发生。
        let event = epitem.ep_item_poll();
        let mut notify_nested = false;
        if !event.is_empty() {
            let mut rs = epoll_guard.ready_state.lock_irqsave();
            let was_empty = rs.ready_list.is_empty();
            if rs.enqueue(&epitem) {
                notify_nested = was_empty;
                rs.epoll_wq.wakeup(None);
            }
        }

        Ok(notify_nested)
    }

    fn ep_remove(
        epoll: &mut MutexGuard<EventPoll>,
        key: EPollKey,
        dst_file: Option<Arc<File>>,
        epitem: &Arc<EPollItem>,
    ) -> Result<(), SystemError> {
        let removed = epoll.ep_items.remove(&key).ok_or(SystemError::ENOENT)?;
        Self::deactivate_and_remove_ready(epoll, &removed);
        if let Some(dst_file) = dst_file {
            let _ = dst_file.remove_epitem(epitem);
        }

        Ok(())
    }

    /// Remove an epoll item when the referenced file is being released.
    ///
    /// This is the DragonOS equivalent of Linux `eventpoll_release_file()`,
    /// called from the file cleanup path rather than from `epoll_ctl(DEL)`.
    pub fn release_file_epitem(epitem: &Arc<EPollItem>) {
        let Some(epoll) = epitem.epoll().upgrade() else {
            return;
        };
        let mut epoll = epoll.lock();
        let key = epitem.key();
        let Some(current) = epoll.ep_items.get(&key).cloned() else {
            return;
        };
        if !Arc::ptr_eq(&current, epitem) {
            return;
        }

        epoll.ep_items.remove(&key);
        Self::deactivate_and_remove_ready(&epoll, &current);
    }

    fn deactivate_and_remove_ready(epoll: &EventPoll, epitem: &Arc<EPollItem>) {
        let retired;
        let overflow;
        {
            let mut rs = epoll.ready_state.lock_irqsave();
            epitem.deactivate();
            retired = rs
                .ready_list
                .extract_if(|registration| registration.item.ptr_eq(&Arc::downgrade(epitem)));
            overflow = rs.ovflist.as_mut().map(|queue| {
                queue.extract_if(|registration| registration.item.ptr_eq(&Arc::downgrade(epitem)))
            });
            let mut links = epitem.links.lock_irqsave();
            links.ready = false;
            links.overflow = false;
        }
        // No scan can coexist with DEL/close while ep->mtx is held. Retiring
        // the charged detached nodes is safe only after releasing ep->lock.
        retire_ready_queue(retired);
        if let Some(queue) = overflow {
            retire_ready_queue(queue);
        }
    }

    /// ## 修改已经注册的监听事件
    ///
    /// ### 参数
    /// - epoll_guard: EventPoll的锁
    /// - epitem: 需要修改的描述符对应的epitem
    /// - event: 新的事件
    fn ep_modify(
        epoll_guard: &mut MutexGuard<EventPoll>,
        epitem: Arc<EPollItem>,
        event: &EPollEvent,
    ) -> Result<bool, SystemError> {
        let mut epi_event_guard = epitem.event.lock_irqsave();

        // 修改epitem
        epi_event_guard.events = event.events;
        epi_event_guard.data = event.data;

        drop(epi_event_guard);
        // 修改后检查文件是否已经有感兴趣事件发生
        let event = epitem.ep_item_poll();
        let mut notify_nested = false;
        if !event.is_empty() {
            let mut rs = epoll_guard.ready_state.lock_irqsave();
            let was_empty = rs.ready_list.is_empty();
            if rs.enqueue(&epitem) {
                notify_nested = was_empty;
                rs.epoll_wq.wakeup(None);
            }
        }
        // TODO:处理EPOLLWAKEUP，目前不支持

        Ok(notify_nested)
    }

    /// ### 判断epoll是否有就绪item（由外层 Mutex 保护的调用路径使用）
    pub fn ep_events_available(&self) -> bool {
        let rs = self.ready_state.lock_irqsave();
        Self::ready_state_has_events(&rs)
    }

    /// Linux 语义：epoll 嵌套需要进行 loop/depth 检查（失败返回 ELOOP）。
    ///
    /// 对齐 Linux 6.6：EP_MAX_NESTS = 4。
    fn ep_loop_check(src: &LockedEventPoll, to: &LockedEventPoll) -> Result<(), SystemError> {
        const EP_MAX_NESTS: usize = 4;

        let mut visited: BTreeSet<usize> = BTreeSet::new();
        let mut stack: Vec<(LockedEventPoll, usize)> = Vec::new();
        stack.push((to.clone(), 0));

        while let Some((cur, depth)) = stack.pop() {
            if depth > EP_MAX_NESTS {
                return Err(SystemError::ELOOP);
            }
            if Arc::ptr_eq(&cur.0, &src.0) {
                return Err(SystemError::ELOOP);
            }

            let key = Arc::as_ptr(&cur.0) as usize;
            if !visited.insert(key) {
                continue;
            }

            let child_items: Vec<Arc<EPollItem>> = {
                let guard = cur.0.lock();
                guard.ep_items.values().cloned().collect()
            };

            for item in child_items {
                let Some(f) = item.file().upgrade() else {
                    continue;
                };
                if !Self::is_epoll_file(&f) {
                    continue;
                }
                let child = match &*f.private_data.lock() {
                    FilePrivateData::EPoll(d) => d.epoll.clone(),
                    _ => continue,
                };
                stack.push((child, depth + 1));
            }
        }

        Ok(())
    }

    /// Processes notifications from files that support epoll.
    ///
    /// Like Linux `ep_poll_callback()`, this path never acquires the outer
    /// sleeping `Mutex<EventPoll>`. Registration and ready-state locks use
    /// irqsave discipline. Source and ready nodes were prepared before
    /// registration publication; the callback path performs no allocation.
    pub fn wakeup_epoll(
        epitems: &EPollItemList,
        pollflags: EPollEventType,
    ) -> Result<(), SystemError> {
        Self::dispatch_notifications(epitems, pollflags, false)
    }

    /// A source broadcast (e.g. the last pipe endpoint closing) must visit
    /// every exclusive registration, independently of callback wake success.
    pub fn wakeup_epoll_all(
        epitems: &EPollItemList,
        pollflags: EPollEventType,
    ) -> Result<(), SystemError> {
        Self::dispatch_notifications(epitems, pollflags, true)
    }

    fn dispatch_notifications(
        epitems: &EPollItemList,
        pollflags: EPollEventType,
        broadcast: bool,
    ) -> Result<(), SystemError> {
        // Release the source lock before any callback or nested notification.
        let through = epitems.last_id();
        // Linux places ordinary callbacks before exclusive callbacks. The
        // classification is immutable: exclusive may only be added, never MOD.
        for exclusive in [false, true] {
            let mut after = 0;
            while let Some((id, item)) = epitems.next(after, through) {
                after = id;
                let Some(epitem) = item.upgrade() else {
                    continue;
                };
                let is_exclusive = epitem.event().lock_irqsave().events()
                    & EPollEventType::EPOLLEXCLUSIVE.bits()
                    != 0;
                if is_exclusive != exclusive {
                    continue;
                }
                if Self::notify_epitem(&epitem, pollflags) && !broadcast {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Publish a single registration and report a consumed source wake quota.
    /// Ordinary callbacks and POLLFREE never stop source traversal.
    fn notify_epitem(epitem: &Arc<EPollItem>, pollflags: EPollEventType) -> bool {
        // 通过 EPollItem 的 ready_state Weak 直接访问 ReadyState — 不需要 Mutex
        let Some(rs_arc) = epitem.ready_state().upgrade() else {
            return false;
        };

        // 读取注册事件掩码（irqsave SpinLock — hardirq-safe）
        let ep_events = {
            let event_guard = epitem.event().lock_irqsave();
            EPollEventType::from_bits_truncate(event_guard.events())
        };

        // 对齐 Linux 6.6 `ep_poll_callback()`：
        // 1) 若该 epitem 不包含任何 poll(2) 事件（仅剩 EP_PRIVATE_BITS），视为"被禁用"
        // 2) 若驱动/文件系统传入了具体的 pollflags（非空），则必须与已注册的事件掩码匹配才入队
        let enabled_mask = ep_events.difference(EPollEventType::EP_PRIVATE_BITS);
        if enabled_mask.is_empty() && !pollflags.contains(EPollEventType::POLLFREE) {
            return false;
        }

        if !pollflags.is_empty()
            && !pollflags.contains(EPollEventType::POLLFREE)
            && pollflags.intersection(ep_events).is_empty()
        {
            return false;
        }

        let consumed = {
            // 仅获取 SpinLock（irqsave）— hardirq-safe
            let mut rs = rs_arc.lock_irqsave();
            if !epitem.is_active() {
                return false;
            }

            rs.enqueue(epitem);

            if ep_events.contains(EPollEventType::EPOLLEXCLUSIVE)
                && !pollflags.contains(EPollEventType::POLLFREE)
            {
                let woken = rs.epoll_wq.wake_one();
                // ep_poll_callback's ewake: combined IN|OUT does not stop
                // source traversal, and an absent/cancelled waiter costs no quota.
                let direction = pollflags.intersection(EPollEventType::EPOLLINOUT_BITS);
                woken
                    && (direction.is_empty()
                        || direction == EPollEventType::EPOLLIN && ep_events.contains(direction)
                        || direction == EPollEventType::EPOLLOUT && ep_events.contains(direction))
            } else {
                rs.epoll_wq.wakeup_all(None);
                false
            }
        };

        if let Some(poll_epitems) = epitem.poll_epitems().upgrade() {
            let _ = Self::wakeup_epoll(
                poll_epitems.as_ref(),
                EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
            );
        }
        consumed
    }
}

/// Source registrations, prepared at epoll_ctl birth. Notification never
/// allocates and invokes callbacks only after releasing the source lock.
#[derive(Debug)]
pub struct EPollItemList {
    // Published nodes own the item until explicit source removal, matching
    // Linux waitqueue registration lifetime across File/epoll close races.
    items: SpinLock<RBTree<u64, Registration>>,
}

impl EPollItemList {
    pub const fn new() -> Self {
        Self {
            items: SpinLock::new(RBTree::new()),
        }
    }

    pub fn add(&self, epitem: Arc<EPollItem>) {
        // All possible source memberships (pipe read/write + File) were
        // prepared and charged before the first publication.
        let mut items = self.items.lock_irqsave();
        if !items.contains_key(&epitem.registration_id) {
            // No links lock is ever held while acquiring a source lock.
            items.insert_prepared(epitem.take_source_node());
        }
    }

    pub fn remove(&self, epitem: &Arc<EPollItem>) -> Result<(), SystemError> {
        let node = self.items.lock_irqsave().extract(&epitem.registration_id);
        match node {
            Some(node) => {
                epitem.return_source_node(node);
                Ok(())
            }
            None => Err(SystemError::ENOENT),
        }
    }

    fn last_id(&self) -> u64 {
        self.items
            .lock_irqsave()
            .get_last()
            .map_or(0, |(id, _)| *id)
    }

    fn next(&self, after: u64, through: u64) -> Option<(u64, Weak<EPollItem>)> {
        let items = self.items.lock_irqsave();
        let (id, registration) = items.upper_bound(&after)?;
        (*id <= through).then(|| (*id, registration.item.clone()))
    }

    pub fn take_all(&self) -> EPollItemDrain {
        EPollItemDrain {
            items: core::mem::take(&mut *self.items.lock_irqsave()),
        }
    }
}

impl Default for EPollItemList {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for EPollItemList {
    fn drop(&mut self) {
        drop(self.take_all());
    }
}

/// Detached source storage: free node memory before its charge receipt.
pub struct EPollItemDrain {
    items: RBTree<u64, Registration>,
}
impl Iterator for EPollItemDrain {
    type Item = Arc<EPollItem>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let id = *self.items.get_first()?.0;
            let (_, mut registration) = self.items.extract(&id).unwrap().into_pair();
            if let Some(item) = registration.published_pin.take() {
                return Some(item);
            }
        }
    }
}
impl Drop for EPollItemDrain {
    fn drop(&mut self) {
        while let Some((&id, _)) = self.items.get_first() {
            drop(self.items.extract(&id).unwrap().into_pair());
        }
    }
}

#[derive(Debug, Clone)]
pub struct LockedEventPoll(pub(super) Arc<Mutex<EventPoll>>);

/// ### Epoll文件的私有信息
#[derive(Debug, Clone)]
pub struct EPollPrivateData {
    pub(super) epoll: LockedEventPoll,
}
