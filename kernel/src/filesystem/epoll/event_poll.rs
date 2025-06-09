use core::{
    fmt::Debug,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    filesystem::vfs::{
        file::{File, FileMode},
        FilePrivateData,
    },
    libs::{
        rbtree::RBTree,
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::WaitQueue,
    },
    process::ProcessManager,
    sched::{schedule, SchedMode},
    time::{
        timer::{next_n_us_timer_jiffies, Timer, WakeUpHelper},
        PosixTimeSpec,
    },
};

use alloc::{
    collections::LinkedList,
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
    /// 是否已经关闭
    shutdown: AtomicBool,
    self_ref: Option<Weak<SpinLock<EventPoll>>>,
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
            let file = ProcessManager::current_pcb()
                .fd_table()
                .read()
                .get_file_by_fd(fd);

            if let Some(file) = file {
                let epitm = self.ep_items.get(&fd).unwrap();
                file.remove_epitem(epitm)?;
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
    pub fn create_epoll(flags: FileMode) -> Result<usize, SystemError> {
        let ep_file = Self::create_epoll_file(flags)?;

        let current_pcb = ProcessManager::current_pcb();
        let fd_table = current_pcb.fd_table();
        let mut fd_table_guard = fd_table.write();

        let fd = fd_table_guard.alloc_fd(ep_file, None)?;

        Ok(fd as usize)
    }

    /// ## 创建epoll文件
    pub fn create_epoll_file(flags: FileMode) -> Result<File, SystemError> {
        if !flags.difference(FileMode::O_CLOEXEC).is_empty() {
            return Err(SystemError::EINVAL);
        }

        // 创建epoll
        let epoll = Self::do_create_epoll();

        // 创建epoll的inode对象
        let epoll_inode = EPollInode::new(epoll.clone());

        let mut ep_file = File::new(
            epoll_inode,
            FileMode::O_RDWR | (flags & FileMode::O_CLOEXEC),
        )?;

        // 设置ep_file的FilePrivateData
        ep_file.private_data = SpinLock::new(FilePrivateData::EPoll(EPollPrivateData { epoll }));
        Ok(ep_file)
    }

    fn do_create_epoll() -> LockedEventPoll {
        let epoll = LockedEventPoll(Arc::new(SpinLock::new(EventPoll::new())));
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

        // 从FilePrivateData获取到epoll
        if let FilePrivateData::EPoll(epoll_data) = &*ep_file.private_data.lock() {
            let mut epoll_guard = {
                if nonblock {
                    // 如果设置非阻塞，则尝试获取一次锁
                    if let Ok(guard) = epoll_data.epoll.0.try_lock_irqsave() {
                        guard
                    } else {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }
                } else {
                    epoll_data.epoll.0.lock_irqsave()
                }
            };

            if op == EPollCtlOption::Add {
                // TODO: 循环检查是否为epoll嵌套epoll的情况，如果是则需要检测其深度
                // 这里是需要一种检测算法的，但是目前未考虑epoll嵌套epoll的情况，所以暂时未实现
                // Linux算法：https://code.dragonos.org.cn/xref/linux-6.1.9/fs/eventpoll.c?r=&mo=56953&fi=2057#2133
                if Self::is_epoll_file(&dst_file) {
                    todo!();
                }
            }

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
                    if ep_item.event.read().events & EPollEventType::EPOLLEXCLUSIVE.bits() != 0 {
                        epds.events |=
                            EPollEventType::EPOLLERR.bits() | EPollEventType::EPOLLHUP.bits();

                        Self::ep_modify(&mut epoll_guard, ep_item, &epds)?;
                    }
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
            let epoll_guard = epoll.0.lock_irqsave();

            let mut timeout = false;
            if let Some(timespec) = timespec {
                if !(timespec.tv_sec > 0 || timespec.tv_nsec > 0) {
                    // 非阻塞情况
                    timeout = true;
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
                    return Self::ep_send_events(epoll.clone(), epoll_event, max_events);
                }

                if epoll.0.lock_irqsave().shutdown.load(Ordering::SeqCst) {
                    // 如果已经关闭
                    return Err(SystemError::EBADF);
                }

                // 如果超时
                if timeout {
                    return Ok(0);
                }

                // 自旋等待一段时间
                available = {
                    let mut ret = false;
                    for _ in 0..50 {
                        if let Ok(guard) = epoll.0.try_lock_irqsave() {
                            if guard.ep_events_available() {
                                ret = true;
                                break;
                            }
                        }
                    }
                    // 最后再次不使用try_lock尝试
                    if !ret {
                        ret = epoll.0.lock_irqsave().ep_events_available();
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
                    return Err(SystemError::ERESTARTSYS);
                }

                // 还未等待到事件发生，则睡眠
                // 注册定时器
                let mut timer = None;
                if let Some(timespec) = timespec {
                    let handle = WakeUpHelper::new(current_pcb.clone());
                    let jiffies = next_n_us_timer_jiffies(
                        (timespec.tv_sec * 1000000 + timespec.tv_nsec / 1000) as u64,
                    );
                    let inner: Arc<Timer> = Timer::new(handle, jiffies);
                    inner.activate();
                    timer = Some(inner);
                }
                let guard = epoll.0.lock_irqsave();
                // 睡眠，等待事件发生
                // 如果wq已经dead，则直接返回错误
                unsafe { guard.epoll_wq.sleep_without_schedule() }.inspect_err(|_| {
                    if let Some(timer) = timer.as_ref() {
                        timer.cancel();
                    }
                })?;
                drop(guard);
                schedule(SchedMode::SM_NONE);

                // 被唤醒后,检查是否有事件可读
                available = epoll.0.lock_irqsave().ep_events_available();
                if let Some(timer) = timer {
                    if timer.as_ref().timeout() {
                        // 超时
                        timeout = true;
                    } else {
                        // 未超时，则取消计时器
                        timer.cancel();
                    }
                }
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
        let mut ep_guard = epoll.0.lock_irqsave();
        let mut res: usize = 0;

        // 在水平触发模式下，需要将epitem再次加入队列，在下次循环再次判断是否还有事件
        // （所以边缘触发的效率会高于水平触发，但是水平触发某些情况下能够使得更迅速地向用户反馈）
        let mut push_back = Vec::new();
        while let Some(epitem) = ep_guard.ready_list.pop_front() {
            if res >= max_events as usize {
                push_back.push(epitem);
                break;
            }
            let ep_events = EPollEventType::from_bits_truncate(epitem.event.read().events);

            // 再次poll获取事件(为了防止水平触发一直加入队列)
            let revents = epitem.ep_item_poll();
            if revents.is_empty() {
                continue;
            }

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

            if ep_events.contains(EPollEventType::EPOLLONESHOT) {
                let mut event_writer = epitem.event.write();
                let new_event = event_writer.events & EPollEventType::EP_PRIVATE_BITS.bits;
                event_writer.set_events(new_event);
            } else if !ep_events.contains(EPollEventType::EPOLLET) {
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
        epoll_guard: &mut SpinLockGuard<EventPoll>,
        dst_file: Arc<File>,
        epitem: Arc<EPollItem>,
    ) -> Result<(), SystemError> {
        if Self::is_epoll_file(&dst_file) {
            return Err(SystemError::ENOSYS);
            // TODO：现在的实现先不考虑嵌套其它类型的文件(暂时只针对socket),这里的嵌套指epoll/select/poll
        }

        let test_poll = dst_file.poll();
        if test_poll.is_err() && test_poll.unwrap_err() == SystemError::EOPNOTSUPP_OR_ENOTSUP {
            // 如果目标文件不支持poll
            return Err(SystemError::ENOSYS);
        }

        epoll_guard.ep_items.insert(epitem.fd, epitem.clone());

        // 检查文件是否已经有事件发生
        let event = epitem.ep_item_poll();
        if !event.is_empty() {
            // 加入到就绪队列
            epoll_guard.ep_add_ready(epitem.clone());

            epoll_guard.ep_wake_one();
        }

        // TODO： 嵌套epoll？

        // 这个标志是用与电源管理相关，暂时不支持
        if epitem.event.read().events & EPollEventType::EPOLLWAKEUP.bits() != 0 {
            return Err(SystemError::ENOSYS);
        }

        dst_file.add_epitem(epitem.clone())?;
        Ok(())
    }

    pub fn ep_remove(
        epoll: &mut SpinLockGuard<EventPoll>,
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
        epoll_guard: &mut SpinLockGuard<EventPoll>,
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
        let ret = self.ready_list.iter().find(|epi| Arc::ptr_eq(epi, &epitem));

        if ret.is_none() {
            self.ready_list.push_back(epitem);
        }
    }

    /// ### 判断该epoll上是否有进程在等待
    pub fn ep_has_waiter(&self) -> bool {
        self.epoll_wq.len() != 0
    }

    /// ### 唤醒所有在epoll上等待的进程
    pub fn ep_wake_all(&self) {
        self.epoll_wq.wakeup_all(None);
    }

    /// ### 唤醒所有在epoll上等待的首个进程
    pub fn ep_wake_one(&self) {
        self.epoll_wq.wakeup(None);
    }

    /// ### epoll的回调，支持epoll的文件有事件到来时直接调用该方法即可
    pub fn wakeup_epoll(
        epitems: &SpinLock<LinkedList<Arc<EPollItem>>>,
        pollflags: EPollEventType,
    ) -> Result<(), SystemError> {
        let epitems_guard = epitems.try_lock_irqsave()?;
        for epitem in epitems_guard.iter() {
            // The upgrade is safe because EventPoll always exists when the epitem is in the list
            let epoll = epitem.epoll().upgrade().unwrap();
            let mut epoll_guard = epoll.try_lock()?;
            let binding = epitem.clone();
            let event_guard = binding.event().read();
            let ep_events = EPollEventType::from_bits_truncate(event_guard.events());
            // 检查事件合理性以及是否有感兴趣的事件
            if !(ep_events
                .difference(EPollEventType::EP_PRIVATE_BITS)
                .is_empty()
                || pollflags.difference(ep_events).is_empty())
            {
                // TODO: 未处理pm相关

                // 首先将就绪的epitem加入等待队列
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
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct LockedEventPoll(pub(super) Arc<SpinLock<EventPoll>>);

/// ### Epoll文件的私有信息
#[derive(Debug, Clone)]
pub struct EPollPrivateData {
    pub(super) epoll: LockedEventPoll,
}
