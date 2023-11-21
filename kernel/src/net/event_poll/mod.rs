use core::{
    fmt::Debug,
    ops::Add,
    sync::atomic::{AtomicBool, Ordering},
};

use alloc::{
    collections::LinkedList,
    sync::{Arc, Weak},
    vec::Vec,
};

use crate::{
    arch::sched::sched,
    filesystem::vfs::{
        file::{File, FileMode},
        FilePrivateData, IndexNode, Metadata,
    },
    include::bindings::bindings::INT32_MAX,
    libs::{
        rbtree::RBTree,
        rwlock::RwLock,
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::WaitQueue,
    },
    mm::VirtAddr,
    process::ProcessManager,
    syscall::{user_access::UserBufferWriter, SystemError},
    time::{
        timer::{next_n_us_timer_jiffies, Timer, WakeUpHelper},
        TimeSpec,
    },
};

pub mod syscall;

#[derive(Debug, Clone)]
pub struct LockedEventPoll(Arc<SpinLock<EventPoll>>);

#[derive(Debug)]
pub struct EventPoll {
    /// epoll_wait用到的等待队列
    epoll_wq: WaitQueue,
    /// 维护所有添加进来的socket的红黑树
    epitem_rbr: RBTree<i32, Arc<EPollItem>>,
    /// 接收就绪的描述符列表
    ready_list: LinkedList<Arc<EPollItem>>,
    shutdown: AtomicBool,
    self_ref: Option<Weak<SpinLock<EventPoll>>>,
}

impl EventPoll {
    pub const EP_MAX_EVENTS: u32 = INT32_MAX / (core::mem::size_of::<EPollEvent>() as u32);
    pub fn new() -> Self {
        Self {
            epoll_wq: WaitQueue::INIT,
            epitem_rbr: RBTree::new(),
            ready_list: LinkedList::new(),
            shutdown: AtomicBool::new(false),
            self_ref: None,
        }
    }
}

#[derive(Debug)]
pub struct EPollItem {
    epoll: Weak<SpinLock<EventPoll>>,
    event: RwLock<EPollEvent>,
    fd: i32,
    file: Weak<SpinLock<File>>,
}

impl EPollItem {
    pub fn new(
        epoll: Weak<SpinLock<EventPoll>>,
        events: EPollEvent,
        fd: i32,
        file: Weak<SpinLock<File>>,
    ) -> Self {
        Self {
            epoll,
            event: RwLock::new(events),
            fd,
            file,
        }
    }

    pub fn epoll(&self) -> Weak<SpinLock<EventPoll>> {
        self.epoll.clone()
    }

    pub fn event(&self) -> &RwLock<EPollEvent> {
        &self.event
    }

    /// ## 通过epoll_item来执行绑定文件的poll方法，并获取到感兴趣的事件
    fn ep_item_poll(&self) -> EPollEventType {
        if let Ok(events) = self.file.upgrade().unwrap().lock().poll() {
            let events = events as u32 & self.event.read().events;
            return EPollEventType::from_bits_truncate(events);
        }
        return EPollEventType::empty();
    }
}

/// ### Epoll文件的私有信息
#[derive(Debug, Clone)]
pub struct EPollPrivateData {
    epoll: LockedEventPoll,
}

/// ### 该结构体将Epoll加入文件系统
#[derive(Debug)]
pub struct EPollInode {
    epoll: LockedEventPoll,
}

impl EPollInode {
    pub fn new(epoll: LockedEventPoll) -> Arc<Self> {
        Arc::new(Self { epoll })
    }
}

impl IndexNode for EPollInode {
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: &mut crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, crate::syscall::SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: &mut crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, crate::syscall::SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn poll(&self) -> Result<usize, crate::syscall::SystemError> {
        // 需要实现epoll嵌套epoll时，需要实现这里
        todo!()
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<Vec<alloc::string::String>, crate::syscall::SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(Metadata::default())
    }

    fn close(&self, _data: &mut FilePrivateData) -> Result<(), SystemError> {
        // 释放资源
        let mut epoll = self.epoll.0.lock_irqsave();

        // 唤醒epoll上面等待的所有进程
        epoll.shutdown.store(true, Ordering::SeqCst);
        epoll.ep_wake_all();

        let fds = epoll.epitem_rbr.keys().cloned().collect::<Vec<_>>();

        // 清理红黑树里面的epitems
        for fd in fds {
            let file = ProcessManager::current_pcb()
                .fd_table()
                .read()
                .get_file_by_fd(fd);

            if file.is_some() {
                file.unwrap()
                    .lock()
                    .remove_epoll(&Arc::downgrade(&self.epoll.0))?;
            }

            epoll.epitem_rbr.remove(&fd);
        }

        Ok(())
    }

    fn open(&self, _data: &mut FilePrivateData, _mode: &FileMode) -> Result<(), SystemError> {
        Ok(())
    }
}

impl EventPoll {
    /// ## 创建epoll对象
    ///
    /// ### 参数
    /// - flags: 创建的epoll文件的FileMode
    ///
    /// ### 返回值
    /// - 成功则返回Ok(fd)，否则返回Err
    pub fn do_create_epoll(flags: FileMode) -> Result<usize, SystemError> {
        if !flags.difference(FileMode::O_CLOEXEC).is_empty() {
            return Err(SystemError::EINVAL);
        }

        // 创建epoll
        let epoll = LockedEventPoll(Arc::new(SpinLock::new(EventPoll::new())));
        epoll.0.lock().self_ref = Some(Arc::downgrade(&epoll.0));

        // 创建epoll的inode对象
        let epoll_inode = EPollInode::new(epoll.clone());

        let mut ep_file = File::new(
            epoll_inode,
            FileMode::O_RDWR | (flags & FileMode::O_CLOEXEC),
        )?;

        // 设置ep_file的FilePrivateData
        ep_file.private_data = FilePrivateData::EPoll(EPollPrivateData { epoll });

        let current_pcb = ProcessManager::current_pcb();
        let fd_table = current_pcb.fd_table();
        let mut fd_table_guard = fd_table.write();

        let fd = fd_table_guard.alloc_fd(ep_file, None)?;

        Ok(fd as usize)
    }

    /// ## epoll_ctl的具体实现
    pub fn do_epoll_ctl(
        epfd: i32,
        op: EPollCtlOption,
        fd: i32,
        epds: &mut EPollEvent,
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
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        // 检查是否允许 EPOLLWAKEUP
        if op != EPollCtlOption::EpollCtlDel {
            epds.events &= !EPollEventType::EPOLLWAKEUP.bits();
        }

        let events = EPollEventType::from_bits_truncate(epds.events);

        // 检查获取到的两个文件的正确性
        // 首先是不能自己嵌套自己
        // 然后ep_file必须是epoll文件
        if Arc::ptr_eq(&ep_file, &dst_file) || !Self::is_epoll_file(&ep_file) {
            return Err(SystemError::EINVAL);
        }

        if op != EPollCtlOption::EpollCtlDel && events.contains(EPollEventType::EPOLLEXCLUSIVE) {
            // epoll独占模式下不允许EpollCtlMod
            if op == EPollCtlOption::EpollCtlMod {
                return Err(SystemError::EINVAL);
            }

            // 不支持嵌套的独占唤醒
            if op == EPollCtlOption::EpollCtlAdd && Self::is_epoll_file(&dst_file)
                || !events
                    .difference(EPollEventType::EPOLLEXCLUSIVE_OK_BITS)
                    .is_empty()
            {
                return Err(SystemError::EINVAL);
            }
        }

        // 从FilePrivateData获取到epoll
        if let FilePrivateData::EPoll(epoll_data) = &ep_file.lock().private_data {
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

            if op == EPollCtlOption::EpollCtlAdd {
                // TODO: 循环检查？
            }

            let ep_item = epoll_guard.epitem_rbr.get(&fd);
            match op {
                EPollCtlOption::EpollCtlAdd => {
                    // 如果已经存在，则返回错误
                    if ep_item.is_some() {
                        return Err(SystemError::EEXIST);
                    }
                    // 设置epoll
                    let epitem = Arc::new(EPollItem::new(
                        Arc::downgrade(&epoll_data.epoll.0),
                        *epds,
                        fd,
                        Arc::downgrade(&dst_file),
                    ));
                    Self::ep_insert(&mut epoll_guard, dst_file, epitem)?;
                }
                EPollCtlOption::EpollCtlDel => {
                    // 不存在则返回错误
                    if ep_item.is_none() {
                        return Err(SystemError::ENOENT);
                    }
                    // 删除
                    Self::ep_remove(&mut epoll_guard, fd, dst_file)?;
                }
                EPollCtlOption::EpollCtlMod => {
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

    /// ## epoll_wait的具体实现
    pub fn do_epoll_wait(
        epfd: i32,
        epoll_event: VirtAddr,
        max_events: i32,
        timespec: Option<TimeSpec>,
    ) -> Result<usize, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        let fd_table = current_pcb.fd_table();
        let fd_table_guard = fd_table.read();

        // 获取epoll文件
        let ep_file = fd_table_guard
            .get_file_by_fd(epfd)
            .ok_or(SystemError::EBADF)?;

        drop(fd_table_guard);

        // 确保是epoll file
        if !Self::is_epoll_file(&ep_file) {
            return Err(SystemError::EINVAL);
        }

        // 从epoll文件获取到epoll
        let mut epolldata = None;
        if let FilePrivateData::EPoll(epoll_data) = &ep_file.lock().private_data {
            epolldata = Some(epoll_data.clone())
        }
        if epolldata.is_some() {
            let epoll_data = epolldata.unwrap();
            let epoll = epoll_data.epoll.clone();
            let epoll_guard = epoll.0.lock_irqsave();

            let mut timeout = false;
            if timespec.is_some() {
                let timespec = timespec.unwrap();
                if !(timespec.tv_sec > 0 || timespec.tv_nsec > 0) {
                    // 非阻塞情况
                    timeout = true;
                }
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

                // 如果有未处理的信号则返回错误
                if current_pcb.sig_info().sig_pending().signal().bits() != 0 {
                    return Err(SystemError::EINTR);
                }

                // 还未等待到事件发生，则睡眠
                // 注册定时器
                let mut timer = None;
                if timespec.is_some() {
                    let timespec = timespec.unwrap();
                    let handle = WakeUpHelper::new(current_pcb.clone());
                    let jiffies = next_n_us_timer_jiffies(
                        (timespec.tv_sec * 1000000000 + timespec.tv_nsec / 1000) as u64,
                    );
                    let inner = Timer::new(handle, jiffies);
                    inner.activate();
                    timer = Some(inner);
                }
                let guard = epoll.0.lock_irqsave();
                unsafe { guard.epoll_wq.sleep_without_schedule() };
                drop(guard);
                sched();
                // 被唤醒后,检查是否有事件可读
                available = epoll.0.lock_irqsave().ep_events_available();
                if timer.is_some() {
                    if timer.as_ref().unwrap().timeout() {
                        // 超时
                        timeout = true;
                    } else {
                        // 未超时，则取消计时器
                        timer.unwrap().cancel();
                    }
                }
            }
        } else {
            panic!("An epoll file does not have the corresponding private information");
        }
    }

    /// ### 将已经准备好的事件拷贝到用户空间
    fn ep_send_events(
        epoll: LockedEventPoll,
        user_event: VirtAddr,
        max_events: i32,
    ) -> Result<usize, SystemError> {
        let mut ep_guard = epoll.0.lock_irqsave();
        let mut res: usize = 0;
        let mut push_back = Vec::new();
        while let Some(epitem) = ep_guard.ready_list.pop_front() {
            if res >= max_events as usize {
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

            // C标准的epoll_event大小为12字节,在内核我们不使用#[repr(packed)]来强制与C兼容，可以提高效率
            let user_addr = user_event.add(res * 12);
            if user_addr.is_null() {
                // 当前指向的地址已为空，则把epitem放回队列
                ep_guard.ready_list.push_back(epitem.clone());
                if res == 0 {
                    // 一个都未写入成功，表明用户传进的地址就是有问题的
                    return Err(SystemError::EFAULT);
                }
            }

            // 拷贝到用户空间
            // 先拷贝events字段
            UserBufferWriter::new(user_addr.as_ptr::<u32>(), core::mem::size_of::<u32>(), true)?
                .copy_one_to_user::<u32>(&event.events, 0)?;
            // 增加偏移量
            let user_addr = user_addr.add(core::mem::size_of::<u32>());
            UserBufferWriter::new(user_addr.as_ptr::<u64>(), core::mem::size_of::<u64>(), true)?
                .copy_one_to_user::<u64>(&event.data, 0)?;
            // 记数加一
            res += 1;

            // crate::kdebug!("ep send {event:?}");

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
    fn is_epoll_file(file: &Arc<SpinLock<File>>) -> bool {
        if let FilePrivateData::EPoll(_) = file.lock().private_data {
            return true;
        }
        return false;
    }

    fn ep_insert(
        epoll_guard: &mut SpinLockGuard<EventPoll>,
        dst_file: Arc<SpinLock<File>>,
        epitem: Arc<EPollItem>,
    ) -> Result<(), SystemError> {
        if Self::is_epoll_file(&dst_file) {
            return Err(SystemError::ENOSYS);
            // TODO：现在的实现先不考虑嵌套其它类型的文件(暂时只针对socket),这里的嵌套指epoll/select/poll
        }

        let test_poll = dst_file.lock().poll();
        if test_poll.is_err() {
            if test_poll.unwrap_err() == SystemError::EOPNOTSUPP_OR_ENOTSUP {
                // 如果目标文件不支持poll
                return Err(SystemError::ENOSYS);
            }
        }

        epoll_guard.epitem_rbr.insert(epitem.fd, epitem.clone());

        // 检查文件是否已经有事件发生
        let event = epitem.ep_item_poll();
        if !event.is_empty() {
            // 加入到就绪队列
            epoll_guard.ep_add_ready(epitem.clone());

            epoll_guard.ep_wake_one();
        }

        drop(epoll_guard);

        // TODO： 嵌套epoll？

        // 这个标志是用与电源管理相关，暂时不支持
        if epitem.event.read().events & EPollEventType::EPOLLWAKEUP.bits() != 0 {
            return Err(SystemError::ENOSYS);
        }

        dst_file.lock().add_epoll(epitem.clone())?;
        Ok(())
    }

    fn ep_remove(
        epoll: &mut SpinLockGuard<EventPoll>,
        fd: i32,
        dst_file: Arc<SpinLock<File>>,
    ) -> Result<(), SystemError> {
        let mut file_guard = dst_file.lock();

        file_guard.remove_epoll(epoll.self_ref.as_ref().unwrap())?;

        let epitem = epoll.epitem_rbr.remove(&fd).unwrap();

        epoll
            .ready_list
            .drain_filter(|item| Arc::ptr_eq(item, &epitem));

        Ok(())
    }

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
}

/// 与C兼容的Epoll事件结构体
#[derive(Copy, Clone, Default)]
pub struct EPollEvent {
    events: u32,
    data: u64,
}

impl Debug for EPollEvent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let events = self.events;
        let u64 = self.data;
        f.debug_struct("epoll_event")
            .field("events", &events)
            .field("data", &u64)
            .finish()
    }
}

impl EPollEvent {
    pub fn set_events(&mut self, events: u32) {
        self.events = events;
    }

    pub fn events(&self) -> u32 {
        self.events
    }
}

#[derive(Debug, PartialEq)]
pub enum EPollCtlOption {
    EpollCtlAdd,
    EpollCtlDel,
    EpollCtlMod,
}

impl EPollCtlOption {
    pub fn from_op_num(op: usize) -> Result<Self, SystemError> {
        match op {
            1 => Ok(Self::EpollCtlAdd),
            2 => Ok(Self::EpollCtlDel),
            3 => Ok(Self::EpollCtlMod),
            _ => Err(SystemError::EINVAL),
        }
    }
}

bitflags! {
    #[allow(dead_code)]
    pub struct EPollEventType: u32 {
        const EPOLLIN = 0x00000001;
        const EPOLLPRI = 0x00000002;
        const EPOLLOUT = 0x00000004;
        const EPOLLERR = 0x00000008;
        const EPOLLHUP = 0x00000010;
        const EPOLLNVAL = 0x00000020;
        const EPOLLRDNORM = 0x00000040;
        const EPOLLRDBAND = 0x00000080;
        const EPOLLWRNORM = 0x00000100;
        const EPOLLWRBAND = 0x00000200;
        const EPOLLMSG = 0x00000400;
        const EPOLLRDHUP = 0x00002000;

        const EPOLL_URING_WAKE = 1u32 << 27;
        const EPOLLEXCLUSIVE = 1u32 << 28;    // 设置epoll为独占模式
        const EPOLLWAKEUP = 1u32 << 29;
        const EPOLLONESHOT = 1u32 << 30;
        const EPOLLET = 1u32 << 31;

        const EPOLLINOUT_BITS = Self::EPOLLIN.bits | Self::EPOLLOUT.bits;
        const EPOLLEXCLUSIVE_OK_BITS =
            Self::EPOLLINOUT_BITS.bits
            | Self::EPOLLERR.bits
            | Self::EPOLLHUP.bits
            | Self::EPOLLWAKEUP.bits
            | Self::EPOLLET.bits
            | Self::EPOLLEXCLUSIVE.bits;

        const EP_PRIVATE_BITS =
            Self::EPOLLWAKEUP.bits
            | Self::EPOLLONESHOT.bits
            | Self::EPOLLET.bits
            | Self::EPOLLEXCLUSIVE.bits;

        const POLLFREE = 0x4000;
    }
}
