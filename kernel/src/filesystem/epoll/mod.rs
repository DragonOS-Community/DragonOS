use super::vfs::file::File;
use crate::libs::{rwlock::RwLock, spinlock::SpinLock};
use alloc::sync::Weak;
use core::fmt::Debug;
use event_poll::EventPoll;
use system_error::SystemError;

pub mod event_poll;
pub mod fs;

/// 与C兼容的Epoll事件结构体
#[derive(Copy, Clone, Default)]
#[repr(packed)]
#[repr(C)]
pub struct EPollEvent {
    /// 表示触发的事件
    events: u32,
    /// 内核态不使用该字段，该字段由用户态自由使用，在事件发生时内核将会原样返回
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

    pub fn set_data(&mut self, data: u64) {
        self.data = data;
    }

    pub fn data(&self) -> u64 {
        self.data
    }
}

/// EpollItem表示的是Epoll所真正管理的对象
/// 每当用户向Epoll添加描述符时都会注册一个新的EpollItem，EpollItem携带了一些被监听的描述符的必要信息
#[derive(Debug)]
pub struct EPollItem {
    /// 对应的Epoll
    epoll: Weak<SpinLock<EventPoll>>,
    /// 用户注册的事件
    event: RwLock<EPollEvent>,
    /// 监听的描述符
    fd: i32,
    /// 对应的文件
    file: Weak<File>,
}

impl EPollItem {
    pub fn new(
        epoll: Weak<SpinLock<EventPoll>>,
        events: EPollEvent,
        fd: i32,
        file: Weak<File>,
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

    pub fn file(&self) -> Weak<File> {
        self.file.clone()
    }

    pub fn fd(&self) -> i32 {
        self.fd
    }

    /// ## 通过epoll_item来执行绑定文件的poll方法，并获取到感兴趣的事件
    fn ep_item_poll(&self) -> EPollEventType {
        let file = self.file.upgrade();
        if file.is_none() {
            return EPollEventType::empty();
        }
        if let Ok(events) = file.unwrap().poll() {
            let events = events as u32 & self.event.read().events;
            return EPollEventType::from_bits_truncate(events);
        }
        return EPollEventType::empty();
    }
}

/// ## epoll_ctl函数的参数
#[derive(Debug, PartialEq)]
pub enum EPollCtlOption {
    /// 注册新的文件描述符到epfd
    Add,
    /// 将对应的文件描述符从epfd中删除
    Del,
    /// 修改已经注册的文件描述符的监听事件
    Mod,
}

impl EPollCtlOption {
    pub fn from_op_num(op: usize) -> Result<Self, SystemError> {
        match op {
            1 => Ok(Self::Add),
            2 => Ok(Self::Del),
            3 => Ok(Self::Mod),
            _ => Err(SystemError::EINVAL),
        }
    }
}

bitflags! {
    #[allow(dead_code)]
    pub struct EPollEventType: u32 {
        /// 对应的描述符有新的数据可读时会触发
        const EPOLLIN = 0x00000001;
        /// 对应的描述符有紧急数据可读时会触发
        const EPOLLPRI = 0x00000002;
        /// 对应的描述符可以写入数据时会触发
        const EPOLLOUT = 0x00000004;
        /// 对应的描述符发生错误时会触发
        const EPOLLERR = 0x00000008;
        /// 对应的描述符被挂断（连接关闭）时会触发
        const EPOLLHUP = 0x00000010;
        /// 对应的描述符不是一个有效的文件描述符时会触发
        const EPOLLNVAL = 0x00000020;
        /// 普通数据可读，类似于`EPOLLIN`
        const EPOLLRDNORM = 0x00000040;
        /// 优先级带外数据可读
        const EPOLLRDBAND = 0x00000080;
        /// 普通数据可写，类似于'EPOLLOUT'
        const EPOLLWRNORM = 0x00000100;
        /// 优先级带外数据可写
        const EPOLLWRBAND = 0x00000200;
        /// 通过消息队列收到消息时会触
        const EPOLLMSG = 0x00000400;
        /// 对应的描述符被挂断（连接关闭）的一端发送了 FIN 时会触发(读关闭)
        const EPOLLRDHUP = 0x00002000;

        /// 以下为额外选项
        ///
        /// 特定选项，用于异步 I/O，目前未实现
        const EPOLL_URING_WAKE = 1u32 << 27;
        /// 设置epoll为独占模式
        const EPOLLEXCLUSIVE = 1u32 << 28;
        ///  允许在系统挂起时唤醒 epoll，通常用于通过 eventfd 或 timerfd 唤醒 epoll,(通常与电源管理相关，未实现)
        const EPOLLWAKEUP = 1u32 << 29;
        /// 表示只监听一次事件，之后需要重新添加
        const EPOLLONESHOT = 1u32 << 30;

        /// 启用边缘触发模式(即只有下次触发事件时才会通过epoll_wait返回)，
        /// 对应为水平触发(默认)，水平触发模式下若这次未处理完数据，那epoll还会将其加入自己的就绪队列
        const EPOLLET = 1u32 << 31;

        /// 以下为组合码
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

        /// 表示epoll已经被释放，但是在目前的设计中未用到
        const POLLFREE = 0x4000;

        /// listen状态的socket可以接受连接
        const EPOLL_LISTEN_CAN_ACCEPT = Self::EPOLLIN.bits | Self::EPOLLRDNORM.bits;
    }
}
