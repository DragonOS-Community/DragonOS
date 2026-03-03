use crate::{
    filesystem::vfs::{
        file::{File, FileFlags},
        FilePrivateData,
    },
    libs::{
        mutex::{Mutex, MutexGuard},
        rbtree::RBTree,
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

/// 内核的Epoll对象结构体，当用户创建一个Epoll时，内核就会创建一个该类型对象
/// 它对应一个epfd
#[derive(Debug)]
pub struct EventPoll {
    /// epoll_wait用到的等待队列
    epoll_wq: WaitQueue,
    /// 维护所有添加进来的socket的红黑树
    ep_items: RBTree<i32, Arc<EPollItem>>,
    /// 接收就绪的描述符列表
    ready_list: LinkedList<Arc<EPollItem>>,
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
            epoll_wq: WaitQueue::default(),
            ep_items: RBTree::new(),
            ready_list: LinkedList::new(),
            poll_epitems: LockedEPItemLinkedList::default(),
            shutdown: AtomicBool::new(false),
            self_ref: None,
        }
    }

    /// 关闭epoll时，执行的逻辑
    pub(super) fn close(&mut self) -> Result<(), SystemError> {
        // 唤醒epoll上面等待的所有进程
        self.shutdown.store(true, Ordering::SeqCst);
        self.ep_wake_all();

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
                    if (ep_item.event.read().events & EPollEventType::EPOLLEXCLUSIVE.bits()) != 0 {
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

            let epoll_guard = epoll.0.lock();

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
            // 判断epoll上有没有就绪事件
            let mut available = epoll_guard.ep_events_available();

            drop(epoll_guard);
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

                // 自旋等待一段时间
                available = {
                    let mut ret = false;
                    for _ in 0..50 {
                        if let Ok(guard) = epoll.0.try_lock() {
                            if guard.ep_events_available() {
                                ret = true;
                                break;
                            }
                        }
                    }
                    // 最后再次不使用try_lock尝试
                    if !ret {
                        ret = epoll.0.lock().ep_events_available();
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
                    let guard = epoll.0.lock();
                    // 注册前再次检查，避免错过事件
                    if guard.ep_events_available() || guard.shutdown.load(Ordering::SeqCst) {
                        available = true;
                        // 不注册，直接继续
                    } else {
                        guard.epoll_wq.register_waker(waker.clone())?;
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
                    let guard = epoll.0.lock();
                    guard.epoll_wq.remove_waker(&waker);
                    available = guard.ep_events_available();
                    if guard.shutdown.load(Ordering::SeqCst) {
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

    /// ## 将已经准备好的事件拷贝到用户空间
    ///
    /// ### 参数
    /// - epoll: 对应的epoll
    /// - user_event: 用户空间传入的epoll_event地址，因为内存对其问题，所以这里需要直接操作地址
    /// - max_events: 处理的最大事件数量
    fn ep_send_events(
        epoll: LockedEventPoll,
        user_event: &mut [EPollEvent],
        max_events: i32,
    ) -> Result<usize, SystemError> {
        if user_event.len() < max_events as usize {
            return Err(SystemError::EINVAL);
        }
        let mut ep_guard = epoll.0.lock();
        let mut res: usize = 0;

        // 在水平触发模式下，需要将epitem再次加入队列，在下次循环再次判断是否还有事件
        // （所以边缘触发的效率会高于水平触发，但是水平触发某些情况下能够使得更迅速地向用户反馈）
        let mut push_back = Vec::new();
        while let Some(epitem) = ep_guard.ready_list.pop_front() {
            if res >= max_events as usize {
                push_back.push(epitem);
                break;
            }
            let revents = epitem.ep_item_poll();

            // 如果没有就绪事件，跳过此项（不报告给用户空间）
            // 这处理了在fd添加到就绪列表后，触发条件已不再满足的情况
            // （例如，数据已被另一个线程消费）
            // 符合Linux 6.6语义：ep_send_events中当revents为0时使用continue
            if revents.is_empty() {
                continue;
            }

            // Linux semantics: epoll_wait(2) returns only ready bits (e.g. EPOLLIN),
            // not control flags like EPOLLET/EPOLLONESHOT.
            let registered = EPollEventType::from_bits_truncate(epitem.event.read().events);
            let is_oneshot = registered.contains(EPollEventType::EPOLLONESHOT);
            let is_edge = registered.contains(EPollEventType::EPOLLET);

            // 构建触发事件结构体
            let event = EPollEvent {
                events: revents.bits,
                data: epitem.event.read().data,
            };

            // 这里是需要判断下一个写入的位置是否为空指针

            // TODO:这里有可能会出现事件丢失的情况
            // 如果用户传入的数组长度小于传入的max_event，到这里时如果已经到数组最大长度，但是未到max_event
            // 会出现的问题是我们会把这个数据写入到后面的内存中，用户无法在传入的数组中拿到事件，而且写脏数据到了后面一片内存，导致事件丢失
            // 出现这个问题的几率比较小，首先是因为用户的使用不规范,后因为前面地址校验是按照max_event来校验的，只会在两块内存连着分配时出现，但是也是需要考虑的

            // 以下的写法判断并无意义，只是记一下错误处理
            // offset += core::mem::size_of::<EPollEvent>();
            // if offset >= max_offset {
            //     // 当前指向的地址已为空，则把epitem放回队列
            //     ep_guard.ready_list.push_back(epitem.clone());
            //     if res == 0 {
            //         // 一个都未写入成功，表明用户传进的地址就是有问题的
            //         return Err(SystemError::EFAULT);
            //     }
            // }

            // 拷贝到用户空间
            user_event[res] = event;
            // 记数加一
            res += 1;

            // crate::debug!("ep send {event:?}");

            if is_oneshot {
                let mut event_writer = epitem.event.write();
                let new_event = event_writer.events & EPollEventType::EP_PRIVATE_BITS.bits;
                event_writer.set_events(new_event);
            } else if !is_edge {
                push_back.push(epitem);
            }
        }

        for item in push_back {
            ep_guard.ep_add_ready(item);
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
        if epitem.event.read().events & EPollEventType::EPOLLWAKEUP.bits() != 0 {
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
        // 注意：必须在 add_epitem 之后检查，以避免以下竞态条件：
        // 1. ep_item_poll 检查事件（无事件）
        // 2. socket 状态变化，notify/wakeup_epoll 被调用
        // 3. 但此时 epitem 尚未加入 epoll_items，wakeup_epoll 找不到它
        // 4. add_epitem 完成
        // 5. poll 线程永远等不到唤醒
        // 通过先 add_epitem 再 poll，即使 notify 在 poll 之前发生，
        // 只要 poll 能检测到已发生的事件，就不会丢失。
        let event = epitem.ep_item_poll();
        if !event.is_empty() {
            epoll_guard.ep_add_ready(epitem.clone());
            epoll_guard.ep_wake_one();
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

        if let Some(epitem) = epoll.ep_items.remove(&fd) {
            epoll.ready_list.retain(|item| !Arc::ptr_eq(item, &epitem));
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
        let mut epi_event_guard = epitem.event.write();

        // 修改epitem
        epi_event_guard.events = event.events;
        epi_event_guard.data = event.data;

        drop(epi_event_guard);
        // 修改后检查文件是否已经有感兴趣事件发生
        let event = epitem.ep_item_poll();
        if !event.is_empty() {
            epoll_guard.ep_add_ready(epitem.clone());

            epoll_guard.ep_wake_one();
        }
        // TODO:处理EPOLLWAKEUP，目前不支持

        Ok(())
    }

    /// ### 判断epoll是否有就绪item
    pub fn ep_events_available(&self) -> bool {
        !self.ready_list.is_empty()
    }

    /// ### 将epitem加入到就绪队列，如果为重复添加则忽略
    pub fn ep_add_ready(&mut self, epitem: Arc<EPollItem>) {
        let was_empty = self.ready_list.is_empty();
        let exists = self.ready_list.iter().any(|epi| Arc::ptr_eq(epi, &epitem));
        if !exists {
            self.ready_list.push_back(epitem);
        }

        // epollfd 在 ready_list 从空 -> 非空时变为可读，需要通知其“父 epoll”。
        if was_empty && !self.ready_list.is_empty() {
            let _ = Self::wakeup_epoll(
                &self.poll_epitems,
                EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
            );
        }
    }

    /// ### 判断该epoll上是否有进程在等待
    pub fn ep_has_waiter(&self) -> bool {
        !self.epoll_wq.is_empty()
    }

    /// ### 唤醒所有在epoll上等待的进程
    pub fn ep_wake_all(&self) {
        self.epoll_wq.wakeup_all(None);
    }

    /// ### 唤醒所有在epoll上等待的首个进程
    pub fn ep_wake_one(&self) {
        self.epoll_wq.wakeup(None);
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
    pub fn wakeup_epoll(
        epitems: &LockedEPItemLinkedList,
        pollflags: EPollEventType,
    ) -> Result<(), SystemError> {
        // 避免持有 `epitems` 锁时再去获取 `epoll` 锁：
        // 其他路径（如 epoll_ctl/注册回调）可能会以 `epoll -> epitems` 的顺序加锁，
        // 若这里反过来会造成 ABBA 死锁。
        //
        // 解决方式：在 `epitems` 锁下复制一份快照，然后释放锁，再逐个处理。
        let epitems_snapshot: Vec<Arc<EPollItem>> = {
            let epitems_guard = epitems.lock();
            epitems_guard.iter().cloned().collect()
        };

        for epitem in epitems_snapshot.iter() {
            // The upgrade is safe because EventPoll always exists when the epitem is in the list
            let Some(epoll) = epitem.epoll().upgrade() else {
                // 如果epoll已经被释放，则直接跳过
                continue;
            };

            // 读取注册事件掩码（不持有 epoll 锁，避免扩大锁粒度）
            let ep_events = {
                let event_guard = epitem.event().read();
                EPollEventType::from_bits_truncate(event_guard.events())
            };

            // 对齐 Linux 6.6 `ep_poll_callback()`：
            // 1) 若该 epitem 不包含任何 poll(2) 事件（仅剩 EP_PRIVATE_BITS），视为“被禁用”（常见于 EPOLLONESHOT 被消费），
            //    直到下一次 EPOLL_CTL_MOD 重新 arm。
            // 2) 若驱动/文件系统传入了具体的 pollflags（非空），则必须与已注册的事件掩码匹配才入队。
            //
            // 参考：linux-6.6.21/fs/eventpoll.c: ep_poll_callback()
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

            // TODO: 未处理pm相关
            let mut epoll_guard = epoll.lock();
            epoll_guard.ep_add_ready(epitem.clone());

            if epoll_guard.ep_has_waiter() {
                if ep_events.contains(EPollEventType::EPOLLEXCLUSIVE)
                    && !pollflags.contains(EPollEventType::POLLFREE)
                {
                    // 避免惊群
                    epoll_guard.ep_wake_one();
                } else {
                    epoll_guard.ep_wake_all();
                }
            }
        }
        Ok(())
    }
}

pub type LockedEPItemLinkedList = Mutex<LinkedList<Arc<EPollItem>>>;

impl Default for LockedEPItemLinkedList {
    fn default() -> Self {
        Mutex::new(LinkedList::new())
    }
}

#[derive(Debug, Clone)]
pub struct LockedEventPoll(pub(super) Arc<Mutex<EventPoll>>);

/// ### Epoll文件的私有信息
#[derive(Debug, Clone)]
pub struct EPollPrivateData {
    pub(super) epoll: LockedEventPoll,
}
