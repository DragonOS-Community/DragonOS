//! Accounted epoll registration entity; event ABI and readiness stay in their modules.
use super::{
    event_poll::{EPollItemList, EPollKey, EventPoll, ReadyState},
    EPollEvent, EPollEventType,
};
use crate::{
    cgroup::memory::{accounting::KernelByteKind, object::ObjectAccount, owned::OwnedBox},
    filesystem::{poll::PollFlags, vfs::file::File},
    libs::{mutex::Mutex, spinlock::SpinLock},
    mm::allocation::MemoryAllocationError,
};
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, Ordering};
use system_error::SystemError;

/// EpollItem表示的是Epoll所真正管理的对象
/// 每当用户向Epoll添加描述符时都会注册一个新的EpollItem，EpollItem携带了一些被监听的描述符的必要信息
#[derive(Debug)]
pub struct EPollItem {
    data: OwnedBox<EPollItemData>,
}

impl core::ops::Deref for EPollItem {
    type Target = EPollItemData;
    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

#[derive(Debug)]
pub struct EPollItemData {
    /// 对应的Epoll
    pub(super) epoll: Weak<Mutex<EventPoll>>,
    /// 直接引用 EventPoll 的 ready_state，使回调路径绕过外层 Mutex。
    /// 对标 Linux 中 ep_poll_callback 仅获取 ep->lock 而不获取 ep->mtx 的设计。
    pub(super) ready_state: Weak<SpinLock<ReadyState>>,
    /// 直接引用 EventPoll 的 poll_epitems，使嵌套 epoll 的向上传播无需获取外层 Mutex。
    pub(super) poll_epitems: Weak<EPollItemList>,
    /// 用户注册的事件
    /// 使用 irqsave SpinLock 而非 RwSem，因为 wakeup_epoll 回调路径可能在
    /// hardirq 上下文中执行（例如 timer IRQ → signal → signalfd → epoll），
    /// RwSem 是可睡眠锁，在 hardirq 中使用会导致死锁。
    /// 对标 Linux 中 epitem.event 由 ep->lock (spin_lock_irqsave) 保护的设计。
    pub(super) event: SpinLock<EPollEvent>,
    /// 对应的文件
    pub(super) file: Weak<File>,
    pub(super) key: EPollKey,
    /// Whether callbacks may still publish this registration to the ready list.
    pub(super) active: AtomicBool,
}

impl EPollItem {
    pub(super) fn try_new(
        creator: &Arc<ObjectAccount>,
        epoll: Weak<Mutex<EventPoll>>,
        ready_state: Weak<SpinLock<ReadyState>>,
        poll_epitems: Weak<EPollItemList>,
        key: EPollKey,
        events: EPollEvent,
        file: Weak<File>,
    ) -> Result<Arc<Self>, MemoryAllocationError> {
        let body = OwnedBox::<EPollItemData>::prepare(creator, KernelByteKind::SlabUnreclaimable)?
            .try_allocate()
            .map_err(|_| SystemError::ENOMEM)?;
        let data = body.init(EPollItemData {
            epoll,
            ready_state,
            poll_epitems,
            key,
            event: SpinLock::new(events),
            file,
            active: AtomicBool::new(true),
        });
        Arc::try_new(Self { data }).map_err(|_| SystemError::ENOMEM.into())
    }

    pub fn epoll(&self) -> Weak<Mutex<EventPoll>> {
        self.epoll.clone()
    }

    /// 获取 ready_state 的 Weak 引用，用于回调路径直接访问就绪状态。
    #[allow(dead_code)]
    pub(crate) fn ready_state(&self) -> Weak<SpinLock<ReadyState>> {
        self.ready_state.clone()
    }

    /// 获取 poll_epitems 的 Weak 引用，用于嵌套 epoll 的向上传播。
    #[allow(dead_code)]
    pub(crate) fn poll_epitems(&self) -> Weak<EPollItemList> {
        self.poll_epitems.clone()
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub(crate) fn deactivate(&self) {
        self.active.store(false, Ordering::Relaxed);
    }

    pub fn event(&self) -> &SpinLock<EPollEvent> {
        &self.event
    }

    pub fn file(&self) -> Weak<File> {
        self.file.clone()
    }

    pub(crate) fn key(&self) -> EPollKey {
        self.key
    }

    /// ## 通过epoll_item来执行绑定文件的poll方法，并获取到感兴趣的事件
    ///
    /// 返回与用户注册的事件掩码相交的就绪事件
    pub(super) fn ep_item_poll(&self) -> EPollEventType {
        let Some(file) = self.file.upgrade() else {
            return EPollEventType::empty();
        };

        // 对于不支持poll的普通文件/目录，返回默认掩码（总是就绪）
        // 需要与用户注册的事件掩码相交，保持与普通文件路径的一致性
        if file.is_always_ready() {
            let interested = self.event.lock_irqsave().events;
            return EPollEventType::from_bits_truncate(
                Self::default_poll_mask().bits() & interested,
            );
        }

        match file.poll() {
            Ok(events) => {
                let interested = self.event.lock_irqsave().events;
                EPollEventType::from_bits_truncate(events as u32 & interested)
            }
            Err(_) => EPollEventType::empty(),
        }
    }

    /// 返回默认的poll掩码，用于不支持poll的普通文件
    ///
    /// 对应Linux内核的 DEFAULT_POLLMASK (POLLIN | POLLOUT | POLLRDNORM | POLLWRNORM)
    #[inline]
    pub(super) fn default_poll_mask() -> EPollEventType {
        PollFlags::DEFAULT_POLLMASK.into()
    }
}
