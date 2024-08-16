use core::{any::Any, fmt::Debug, sync::atomic::AtomicUsize};

use alloc::{
    boxed::Box,
    collections::LinkedList,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use log::warn;
use smoltcp::{
    iface::SocketSet,
    socket::{self, raw, tcp, udp},
};
use system_error::SystemError;

use crate::{
    arch::rand::rand, driver::net::Iface, filesystem::vfs::{
        file::FileMode, syscall::ModeType, FilePrivateData, FileSystem, FileType, IndexNode,
        Metadata,
    }, libs::{
        rwlock::{RwLock, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::EventWaitQueue,
    }, process::{Pid, ProcessManager}, sched::{schedule, SchedMode}
};

use super::{
    handle::GlobalSocketHandle, inet::{RawSocket, TcpSocket, BoundUdp}, unix::{SeqpacketSocket, StreamSocket}, Socket, Options, InetSocketType, PORT_MANAGER
};

use super::super::{
    event_poll::{EPollEventType, EPollItem, EventPoll}, Endpoint, Protocol, ShutdownType, SocketOptionsLevel
};

/// # Socket在文件系统中的inode封装
#[derive(Debug)]
pub struct SocketInode {
    bound_iface: Option<Arc<dyn Iface>>,
    /// socket的实现
    socket: SpinLock<Box<dyn Socket>>,
    /// following are socket commons
    is_listen: bool,

    shutdown_type: RwLock<ShutdownType>,

    epoll_item: SpinLock<LinkedList<Arc<EPollItem>>>,

    wait_queue: Arc<EventWaitQueue>,
}

impl SocketInode {
    pub fn new(socket: Box<dyn Socket>, wait_queue: Option<Arc<EventWaitQueue>>) -> Arc<Self> {
        Arc::new(Self {
            bound_iface: None,
            socket: SpinLock::new(socket),
            is_listen: false,
            epoll_item: SpinLock::new(LinkedList::new()),
            shutdown_type: RwLock::new(ShutdownType::empty()),
            wait_queue: wait_queue.unwrap_or(Arc::new(EventWaitQueue::new())),
        })
    }

    #[inline]
    pub fn inner(&self) -> SpinLockGuard<Box<dyn Socket>> {
        self.socket.lock()
    }

    pub unsafe fn inner_no_preempt(&self) -> SpinLockGuard<Box<dyn Socket>> {
        self.socket.lock_no_preempt()
    }

    // ==> epoll api
    pub fn add_epoll(&self, epitem: Arc<EPollItem>) {
        self.epoll_item.lock_irqsave().push_back(epitem)
    }

    pub fn remove_epoll(&self, epoll: &Weak<SpinLock<EventPoll>>) -> Result<(), SystemError> {
        let is_remove = !self
            .epoll_item
            .lock_irqsave()
            .extract_if(|x| x.epoll().ptr_eq(epoll))
            .collect::<Vec<_>>()
            .is_empty();

        if is_remove {
            return Ok(());
        }

        Err(SystemError::ENOENT)
    }

    fn clear_epoll(&self) -> Result<(), SystemError> {
        for epitem in self.epoll_item.lock_irqsave().iter() {
            let epoll = epitem.epoll();

            if let Some(epoll) = epoll.upgrade() {
                EventPoll::ep_remove(&mut epoll.lock_irqsave(), epitem.fd(), None)?;
            }
        }

        Ok(())
    }
    // <== epoll api

    /// # wakeup_any
    /// 唤醒该队列上等待events的进程
    /// ## 参数
    /// - events: 发生的事件
    /// ## Notice
    /// 只要触发了events中的任意一件事件，进程都会被唤醒
    pub fn wakeup_any(&self, events: u64) {
        self.wait_queue.wakeup_any(events);
    }

    /// ## 在socket的等待队列上睡眠
    pub fn sleep(&self, events: u64) {
        unsafe {
            ProcessManager::preempt_disable();
            self.wait_queue.sleep_without_schedule(events);
            ProcessManager::preempt_enable();
        }
        schedule(SchedMode::SM_NONE);
    }

    // ==> shutdown_type api
    pub fn shutdown_type(&self) -> ShutdownType {
        *self.shutdown_type.read()
    }

    pub fn shutdown_type_writer(&mut self) -> RwLockWriteGuard<ShutdownType> {
        self.shutdown_type.write_irqsave()
    }

    pub fn reset_shutdown_type(&self) {
        *self.shutdown_type.write() = ShutdownType::empty();
    }
    // <== shutdown_type api
}

impl IndexNode for SocketInode {
    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _mode: &FileMode,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        let mut socket = self.socket.lock_irqsave();

        if socket.metadata().socket_type == InetSocketType::Unix {
            return Ok(());
        }

        self.clear_epoll()?;

        socket.close();

        if let Some(iface) = self.bound_iface.as_ref() {
            if let Some(Endpoint::Ip(Some(ip))) = socket.endpoint() {
                iface.port_manager().unbind_port(socket.metadata().socket_type, ip.port);
            }
            iface.poll()?;
        }

        Ok(())
    }

    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        drop(data);

        let read_result = loop {
            if let Some(iface) = self.bound_iface.as_ref() {
                iface.poll()?;
            }
            let read_result
                = self.socket.lock_no_preempt().read(&mut buf[0..len]);
            if self
                .socket
                .lock()
                .metadata()
                .options
                .contains(Options::BLOCK) 
            {
                match read_result {
                    Ok((x, _)) => break Ok(x),
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        self.sleep(EPollEventType::EPOLLIN.bits() as u64);
                        continue;
                    }
                    Err(e) => break Err(e),
                }
            }
        };
        if let Some(iface) = self.bound_iface.as_ref() {
            iface.poll()?;
        }
        return read_result;
    }

    fn write_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &[u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        drop(data);
        let write_len = self.socket.lock_no_preempt().write(&buf[0..len], None)?;
        if let Some(iface) = self.bound_iface.as_ref() {
            iface.poll()?;
        }
        return Ok(write_len);
    }

    fn poll(&self, _private_data: &FilePrivateData) -> Result<usize, SystemError> {
        let mut events = self.socket.lock_irqsave().poll();
        if self.shutdown_type().contains(ShutdownType::RCV_SHUTDOWN) {
            events.insert(
                EPollEventType::EPOLLRDHUP | EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
            );
        }
        if self.shutdown_type().contains(ShutdownType::SHUTDOWN_MASK) {
            events.insert(EPollEventType::EPOLLHUP);
        }

        return Ok(events.bits() as usize);
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        return Err(SystemError::ENOTDIR);
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        let meta = Metadata {
            mode: ModeType::from_bits_truncate(0o755),
            file_type: FileType::Socket,
            ..Default::default()
        };

        return Ok(meta);
    }

    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        return Ok(());
    }
}