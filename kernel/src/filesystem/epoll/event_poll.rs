use crate::{
    filesystem::vfs::{
        file::{File, FileFlags},
        FilePrivateData,
    },
    libs::{
        mutex::{Mutex, MutexGuard},
        rbtree::RBTree,
        spinlock::SpinLock,
        wait_queue::{TimeoutWaker, WaitQueue, Waiter},
    },
    process::ProcessManager,
    time::{
        timer::{next_n_us_timer_jiffies, Timer},
        Duration, Instant, PosixTimeSpec,
    },
};
use core::{
    fmt::Debug,
    sync::atomic::{AtomicBool, Ordering},
};

use alloc::{
    collections::{BTreeSet, LinkedList},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use super::{fs::EPollInode, EPollCtlOption, EPollEvent, EPollEventType, EPollItem};

/// epoll 就绪状态，由独立的 irqsave SpinLock 保护。
///
/// 对标 Linux 6.6 中由 `ep->lock`（rwlock_t）保护的 `rdllist` + `ovflist` + `wq`。
/// 回调路径（`wakeup_epoll`，等价于 Linux `ep_poll_callback`）仅获取此 SpinLock，
/// 不碰外层 Mutex，因此完全 hardirq-safe。
pub(crate) struct ReadyState {
    /// 就绪列表（正常路径）
    ready_list: LinkedList<Arc<EPollItem>>,
    /// 溢出列表 — 在 `ep_send_events` 扫描期间累积回调事件。
    /// - `None` = 正常模式（回调直接加入 ready_list）
    /// - `Some(vec)` = 扫描模式（回调推入 ovflist）
    ///
    /// 对标 Linux `ep->ovflist`：
    /// - `EP_UNACTIVE_PTR` (-1) ↔ `None`
    /// - `NULL` / chain ↔ `Some(vec)`
    ovflist: Option<Vec<Arc<EPollItem>>>,
    /// epoll_wait 等待者
    epoll_wq: WaitQueue,
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
    ep_items: RBTree<i32, Arc<EPollItem>>,
    /// 就绪状态（由内层 irqsave SpinLock 保护）
    ready_state: Arc<SpinLock<ReadyState>>,
    /// 监听本 epollfd 的 epitems（用于支持 epoll 嵌套：epollfd 被加入另一个 epoll）
    pub(super) poll_epitems: LockedEPItemLinkedList,
    /// 是否已经关闭
    shutdown: AtomicBool,
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
                ready_list: LinkedList::new(),
                ovflist: None,
                epoll_wq: WaitQueue::default(),
            })),
            poll_epitems: LockedEPItemLinkedList::default(),
            shutdown: AtomicBool::new(false),
            self_ref: None,
        }
    }

    /// 关闭epoll时，执行的逻辑
    pub(super) fn close(&mut self) -> Result<(), SystemError> {
        // 唤醒epoll上面等待的所有进程
        self.shutdown.store(true, Ordering::SeqCst);
        {
            let rs = self.ready_state.lock_irqsave();
            rs.epoll_wq.wakeup_all(None);
        }

        let fds: Vec<i32> = self.ep_items.keys().cloned().collect::<Vec<_>>();
        // 清理红黑树里面的epitems
        for fd in fds {
            let fdtable = ProcessManager::current_pcb().basic().try_fd_table().clone();
            let file = fdtable.and_then(|fdtable| fdtable.read().get_file_by_fd(fd));

            if let Some(file) = file {
                let epitm = self.ep_items.get(&fd).unwrap();
                // 尝试移除epitem，忽略错误（对于普通文件，我们没有添加epitem，所以会失败）
                let _ = file.remove_epitem(epitm);
            }
            self.ep_items.remove(&fd);
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
        let mut fd_table_guard = fd_table.write();

        let fd = fd_table_guard.alloc_fd(ep_file, None, cloexec)?;

        Ok(fd as usize)
    }

    /// ## 创建epoll文件
    pub fn create_epoll_file(flags: FileFlags) -> Result<File, SystemError> {
        if !flags.difference(FileFlags::O_CLOEXEC).is_empty() {
            return Err(SystemError::EINVAL);
        }

        // 创建epoll
        let epoll = Self::do_create_epoll();

        // 创建epoll的inode对象
        let epoll_inode = EPollInode::new(epoll.clone());

        let mut ep_file = File::new(
            epoll_inode,
            FileFlags::O_RDWR | (flags & FileFlags::O_CLOEXEC),
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
                || !events
                    .difference(EPollEventType::EPOLLEXCLUSIVE_OK_BITS)
                    .is_empty()
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

        {
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

            let ep_item = epoll_guard.ep_items.get(&dstfd).cloned();
            match op {
                EPollCtlOption::Add => {
                    // 如果已经存在，则返回错误
                    if ep_item.is_some() {
                        return Err(SystemError::EEXIST);
                    }

                    // 设置epoll
                    let epitem = Arc::new(EPollItem::new(
                        Arc::downgrade(&epoll_data.epoll.0),
                        Arc::downgrade(&epoll_guard.ready_state),
                        epds,
                        dstfd,
                        Arc::downgrade(&dst_file),
                    ));
                    Self::ep_insert(&mut epoll_guard, dst_file, epitem)?;
                }
                EPollCtlOption::Del => {
                    match ep_item {
                        Some(ref ep_item) => {
                            // 删除
                            Self::ep_remove(&mut epoll_guard, dstfd, Some(dst_file), ep_item)?;
                        }
                        None => {
                            // 不存在则返回错误
                            return Err(SystemError::ENOENT);
                        }
                    }
                }
                EPollCtlOption::Mod => {
                    // 不存在则返回错误
                    if ep_item.is_none() {
                        return Err(SystemError::ENOENT);
                    }
                    let ep_item = ep_item.unwrap().clone();

                    // EPOLLEXCLUSIVE 只能在 ADD 时设置；如果已有该位，MOD 需要保留它。
                    if (ep_item.event.lock_irqsave().events & EPollEventType::EPOLLEXCLUSIVE.bits())
                        != 0
                    {
                        epds.events |= EPollEventType::EPOLLEXCLUSIVE.bits();
                    }

                    Self::ep_modify(&mut epoll_guard, ep_item, &epds)?;
                }
            }
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
        epoll_event: &mut [EPollEvent],
        max_events: i32,
        timespec: Option<PosixTimeSpec>,
    ) -> Result<usize, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        let fd_table = current_pcb.fd_table();
        let fd_table_guard = fd_table.read();

        // 获取epoll文件
        let ep_file = fd_table_guard
            .get_file_by_fd(epfd)
            .ok_or(SystemError::EBADF)?;

        drop(fd_table_guard);
        Self::epoll_wait_with_file(ep_file, epoll_event, max_events, timespec)
    }

    /// ## epoll_wait的具体实现
    pub fn epoll_wait_with_file(
        ep_file: Arc<File>,
        epoll_event: &mut [EPollEvent],
        max_events: i32,
        timespec: Option<PosixTimeSpec>,
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

            let mut timeout = false;
            let mut deadline: Option<Instant> = None;
            if let Some(timespec) = timespec {
                if !(timespec.tv_sec > 0 || timespec.tv_nsec > 0) {
                    // 非阻塞情况
                    timeout = true;
                } else {
                    let timeout_us =
                        (timespec.tv_sec * 1_000_000 + timespec.tv_nsec / 1_000) as u64;
                    deadline = Some(Instant::now() + Duration::from_micros(timeout_us));
                }
            } else if timespec.is_none() {
                // 非阻塞情况
                timeout = false;
            }
            // 判断epoll上有没有就绪事件（仅需 SpinLock）
            let mut available = Self::ep_events_available_rs(&rs_arc);

            loop {
                if available {
                    // 如果有就绪的事件，则直接返回就绪事件
                    let sent = Self::ep_send_events(epoll.clone(), epoll_event, max_events)?;

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

                if epoll.0.lock().shutdown.load(Ordering::SeqCst) {
                    // 如果已经关闭
                    return Err(SystemError::EBADF);
                }

                // 如果超时
                if timeout {
                    return Ok(0);
                }

                if let Some(deadline) = deadline {
                    if Instant::now() >= deadline {
                        return Ok(0);
                    }
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

                // 如果有未处理且未被屏蔽的信号则返回错误
                if current_pcb.has_pending_signal_fast()
                    && current_pcb.has_pending_not_masked_signal()
                {
                    // Linux epoll_wait(2): interrupted by signal handler -> EINTR.
                    // Returning ERESTARTSYS would cause userspace to restart the syscall
                    // (SA_RESTART), which breaks gVisor's UnblockWithSignal expectation.
                    return Err(SystemError::EINTR);
                }

                // 还未等待到事件发生，则睡眠
                // 构造一次等待（先构造 Waiter/Waker，超时需要通过 Waker::wake 触发）
                let (waiter, waker) = Waiter::new_pair();

                // 注册定时器：用 waker.wake() 来触发 waiter 退出等待（而不是仅唤醒 PCB）
                let mut timer = None;
                if let Some(deadline) = deadline {
                    let remain = deadline.saturating_sub(Instant::now());
                    if remain == Duration::ZERO {
                        timeout = true;
                    } else {
                        let jiffies = next_n_us_timer_jiffies(remain.total_micros());
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
                    if !rs.ready_list.is_empty() || epoll.0.lock().shutdown.load(Ordering::SeqCst) {
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
                    available = !rs.ready_list.is_empty();
                    if epoll.0.lock().shutdown.load(Ordering::SeqCst) {
                        // epoll 被关闭，直接退出
                        return Err(SystemError::EINVAL);
                    }
                }

                if let Some(timer) = timer {
                    if timer.as_ref().timeout() {
                        timeout = true;
                    } else {
                        timer.cancel();
                    }
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
        !rs.ready_list.is_empty()
    }

    /// 开始就绪列表扫描：偷取 ready_list 到返回的 Vec 中，
    /// 激活 ovflist 使回调期间的新事件进入溢出列表。
    ///
    /// 对标 Linux `ep_start_scan()`。调用者必须持有外层 Mutex。
    fn ep_start_scan(&self) -> Vec<Arc<EPollItem>> {
        let mut rs = self.ready_state.lock_irqsave();
        let mut stolen = Vec::new();
        while let Some(item) = rs.ready_list.pop_front() {
            stolen.push(item);
        }
        rs.ovflist = Some(Vec::new());
        stolen
    }

    /// 结束就绪列表扫描：将 ovflist 排空回 ready_list，
    /// 将剩余的扫描项（水平触发需要重入队的）也加回去，关闭 ovflist。
    ///
    /// 对标 Linux `ep_done_scan()`。调用者必须持有外层 Mutex。
    fn ep_done_scan(&self, remaining: Vec<Arc<EPollItem>>) {
        let mut rs = self.ready_state.lock_irqsave();

        // 将扫描期间溢出列表中积累的事件合并回 ready_list
        if let Some(ovf) = rs.ovflist.take() {
            for epi in ovf {
                if !rs.ready_list.iter().any(|e| Arc::ptr_eq(e, &epi)) {
                    rs.ready_list.push_back(epi);
                }
            }
        }
        // ovflist 已经是 None（关闭溢出模式）

        // 将水平触发等需要重入队的项加回 ready_list
        for epi in remaining {
            if !rs.ready_list.iter().any(|e| Arc::ptr_eq(e, &epi)) {
                rs.ready_list.push_back(epi);
            }
        }

        // 如果 ready_list 非空且有等待者，唤醒他们
        if !rs.ready_list.is_empty() {
            rs.epoll_wq.wakeup_all(None);
        }
    }

    /// ## 将已经准备好的事件拷贝到用户空间
    ///
    /// 使用三阶段扫描协议（对标 Linux ep_send_events + ep_start/done_scan）：
    /// 1. ep_start_scan: 偷取 ready_list，激活 ovflist
    /// 2. 遍历偷取的列表，调用 ep_item_poll 收集事件（此时回调进入 ovflist）
    /// 3. ep_done_scan: 将 ovflist 合并回 ready_list，重入队水平触发项
    fn ep_send_events(
        epoll: LockedEventPoll,
        user_event: &mut [EPollEvent],
        max_events: i32,
    ) -> Result<usize, SystemError> {
        if user_event.len() < max_events as usize {
            return Err(SystemError::EINVAL);
        }
        let ep_guard = epoll.0.lock();
        let mut res: usize = 0;

        // Phase 1: 偷取 ready_list，激活 ovflist
        let stolen = ep_guard.ep_start_scan();

        // Phase 2: 遍历偷取的列表（此时 ovflist 吸收并发回调）
        let mut push_back = Vec::new();
        for epitem in stolen {
            if res >= max_events as usize {
                push_back.push(epitem);
                break;
            }
            let revents = epitem.ep_item_poll();

            // 如果没有就绪事件，跳过此项
            if revents.is_empty() {
                continue;
            }

            let registered = EPollEventType::from_bits_truncate(epitem.event.lock_irqsave().events);
            let is_oneshot = registered.contains(EPollEventType::EPOLLONESHOT);
            let is_edge = registered.contains(EPollEventType::EPOLLET);

            let event = EPollEvent {
                events: revents.bits,
                data: epitem.event.lock_irqsave().data,
            };

            user_event[res] = event;
            res += 1;

            if is_oneshot {
                let mut event_guard = epitem.event.lock_irqsave();
                let new_event = event_guard.events & EPollEventType::EP_PRIVATE_BITS.bits;
                event_guard.set_events(new_event);
            } else if !is_edge {
                push_back.push(epitem);
            }
        }

        // Phase 3: 将 ovflist 合并回 ready_list，重入队水平触发项
        ep_guard.ep_done_scan(push_back);

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
    ) -> Result<(), SystemError> {
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

        epoll_guard.ep_items.insert(epitem.fd, epitem.clone());

        // 先将 epitem 添加到目标文件的 epoll_items 中，这样之后的 notify/wakeup_epoll
        // 才能找到并唤醒这个 epitem。
        if let Err(e) = dst_file.add_epitem(epitem.clone()) {
            // 如果添加失败，需要清理 ep_items 中已插入的项
            epoll_guard.ep_items.remove(&epitem.fd);
            return Err(e);
        }

        // 现在检查文件是否已经有事件发生。
        let event = epitem.ep_item_poll();
        if !event.is_empty() {
            let mut rs = epoll_guard.ready_state.lock_irqsave();
            if !rs.ready_list.iter().any(|e| Arc::ptr_eq(e, &epitem)) {
                let was_empty = rs.ready_list.is_empty();
                rs.ready_list.push_back(epitem.clone());
                // 通知嵌套 epoll（如果 ready_list 从空变非空）
                if was_empty {
                    drop(rs);
                    let _ = Self::wakeup_epoll(
                        &epoll_guard.poll_epitems,
                        EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
                    );
                    let rs2 = epoll_guard.ready_state.lock_irqsave();
                    rs2.epoll_wq.wakeup(None);
                } else {
                    rs.epoll_wq.wakeup(None);
                }
            }
        }

        Ok(())
    }

    pub fn ep_remove(
        epoll: &mut MutexGuard<EventPoll>,
        fd: i32,
        dst_file: Option<Arc<File>>,
        epitem: &Arc<EPollItem>,
    ) -> Result<(), SystemError> {
        if let Some(dst_file) = dst_file {
            dst_file.remove_epitem(epitem)?;
        }

        if let Some(removed) = epoll.ep_items.remove(&fd) {
            let mut rs = epoll.ready_state.lock_irqsave();
            rs.ready_list.retain(|item| !Arc::ptr_eq(item, &removed));
        }

        Ok(())
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
    ) -> Result<(), SystemError> {
        let mut epi_event_guard = epitem.event.lock_irqsave();

        // 修改epitem
        epi_event_guard.events = event.events;
        epi_event_guard.data = event.data;

        drop(epi_event_guard);
        // 修改后检查文件是否已经有感兴趣事件发生
        let event = epitem.ep_item_poll();
        if !event.is_empty() {
            let mut rs = epoll_guard.ready_state.lock_irqsave();
            if !rs.ready_list.iter().any(|e| Arc::ptr_eq(e, &epitem)) {
                let was_empty = rs.ready_list.is_empty();
                rs.ready_list.push_back(epitem.clone());
                if was_empty {
                    drop(rs);
                    let _ = Self::wakeup_epoll(
                        &epoll_guard.poll_epitems,
                        EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
                    );
                    let rs2 = epoll_guard.ready_state.lock_irqsave();
                    rs2.epoll_wq.wakeup(None);
                } else {
                    rs.epoll_wq.wakeup(None);
                }
            }
        }
        // TODO:处理EPOLLWAKEUP，目前不支持

        Ok(())
    }

    /// ### 判断epoll是否有就绪item（由外层 Mutex 保护的调用路径使用）
    pub fn ep_events_available(&self) -> bool {
        let rs = self.ready_state.lock_irqsave();
        !rs.ready_list.is_empty()
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

    /// ### epoll的回调，支持epoll的文件有事件到来时直接调用该方法即可
    ///
    /// 对标 Linux `ep_poll_callback()`。仅获取内层 SpinLock（irqsave），
    /// **不获取外层 Mutex**，因此完全 hardirq-safe。
    ///
    /// 回调路径通过 `EPollItem::ready_state()` 直接访问 `ReadyState`，
    /// 绕过 `Mutex<EventPoll>`。
    pub fn wakeup_epoll(
        epitems: &LockedEPItemLinkedList,
        pollflags: EPollEventType,
    ) -> Result<(), SystemError> {
        // 在 epitems 锁下复制一份快照，然后释放锁，再逐个处理。
        // 避免持有 epitems 锁时再去获取其他锁导致 ABBA 死锁。
        let epitems_snapshot: Vec<Arc<EPollItem>> = {
            let epitems_guard = epitems.lock_irqsave();
            epitems_guard.iter().cloned().collect()
        };

        for epitem in epitems_snapshot.iter() {
            // 通过 EPollItem 的 ready_state Weak 直接访问 ReadyState — 不需要 Mutex
            let Some(rs_arc) = epitem.ready_state().upgrade() else {
                continue;
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
                continue;
            }

            if !pollflags.is_empty()
                && !pollflags.contains(EPollEventType::POLLFREE)
                && pollflags.intersection(ep_events).is_empty()
            {
                continue;
            }

            // 仅获取 SpinLock（irqsave）— hardirq-safe
            let mut rs = rs_arc.lock_irqsave();

            if let Some(ref mut ovflist) = rs.ovflist {
                // 扫描进行中 — 推入溢出列表
                if !ovflist.iter().any(|e| Arc::ptr_eq(e, epitem)) {
                    ovflist.push(epitem.clone());
                }
            } else {
                // 正常模式 — 推入 ready_list
                if !rs.ready_list.iter().any(|e| Arc::ptr_eq(e, epitem)) {
                    rs.ready_list.push_back(epitem.clone());
                }
            }

            if ep_events.contains(EPollEventType::EPOLLEXCLUSIVE)
                && !pollflags.contains(EPollEventType::POLLFREE)
            {
                // 避免惊群
                rs.epoll_wq.wakeup(None);
            } else {
                rs.epoll_wq.wakeup_all(None);
            }
        }
        Ok(())
    }
}

/// LockedEPItemLinkedList — 使用 irqsave SpinLock 保护的 epitem 链表。
///
/// 从 Mutex 改为 SpinLock 使得 `wakeup_epoll` 中的快照操作在 hardirq
/// 上下文中也是安全的。链表操作（push_back、retain、snapshot clone）
/// 临界区都很短，适合 SpinLock。
pub type LockedEPItemLinkedList = SpinLock<LinkedList<Arc<EPollItem>>>;

impl Default for LockedEPItemLinkedList {
    fn default() -> Self {
        SpinLock::new(LinkedList::new())
    }
}

#[derive(Debug, Clone)]
pub struct LockedEventPoll(pub(super) Arc<Mutex<EventPoll>>);

/// ### Epoll文件的私有信息
#[derive(Debug, Clone)]
pub struct EPollPrivateData {
    pub(super) epoll: LockedEventPoll,
}
