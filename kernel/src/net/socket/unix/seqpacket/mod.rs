mod inner;
use hashbrown::HashMap;
use inner::Status;
use alloc::{
    string::String,
    sync::{Arc, Weak},
    collections::BTreeMap,
};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::{
    filesystem::vfs::{IndexNode, InodeId, PollableInode}, libs::{rwlock::RwLock, spinlock::SpinLock, wait_queue::WaitQueue}, net::socket::{unix::{ns::{AbstractUnixPath, UnixSockMap}, Unix, UnixEndpoint}, Socket, PMSG}
};
use crate::{
    net::{
        posix::MsgHdr,
        socket::{
            common::shutdown::{Shutdown, ShutdownBit},
            endpoint::Endpoint,
        },
    },
    sched::SchedMode,
};

use system_error::SystemError;

type EP = crate::filesystem::epoll::EPollEventType;

lazy_static! {
    pub static ref SEQ_MAP: UnixSockMap<SeqpacketSocket> = UnixSockMap::new();
}

#[derive(Debug)]
pub struct SeqpacketSocket {
    inner: SpinLock<inner::Status>,
    shutdown: Shutdown,
    is_nonblocking: AtomicBool,
    wait_queue: WaitQueue,
    self_ref: Weak<Self>,
}

impl SeqpacketSocket {
    /// 默认的缓冲区大小
    pub const DEFAULT_BUF_SIZE: usize = 64 * 1024;

    pub fn new(is_nonblocking: bool) -> Arc<dyn Socket> {
        Arc::new_cyclic(|me| { Self {
            inner: SpinLock::new(Status::Init),
            shutdown: Shutdown::new(),
            is_nonblocking: AtomicBool::new(is_nonblocking),
            wait_queue: WaitQueue::default(),
            self_ref: me.clone(),
        }})
    }

    // /*
    //     unnamed
    //     A stream socket that has not been bound to a pathname using
    //     bind(2) has no name. Likewise, the two sockets created by
    //     socketpair(2) are unnamed.  When the address of an unnamed
    //     socket is returned, its length is sizeof(sa_family_t), and
    //     sun_path should not be inspected.
    //  */
    // pub fn new_pairs(is_nonblocking: bool) -> Result<(Arc<dyn Socket>, Arc<dyn Socket>), SystemError> {
    //     let inodea = Arc::new_cyclic(|me| { Self {
    //             peer: OnceLock::new(),
    //             shutdown: Shutdown::new(),
    //             is_nonblocking: AtomicBool::new(is_nonblocking),
    //             wait_queue: WaitQueue::default(),
    //             self_ref: me.clone(),
    //         }});
    //     let inodeb = Arc::new_cyclic(|me| { Self {
    //             peer: OnceLock::new(),
    //             shutdown: Shutdown::new(),
    //             is_nonblocking: AtomicBool::new(is_nonblocking),
    //             wait_queue: WaitQueue::default(),
    //             self_ref: me.clone(),
    //         }});

    //     inodea.peer.set(Arc::downgrade(&inodeb));
    //     inodeb.peer.set(Arc::downgrade(&inodea));

    //     let inode0 = SocketInode::new(inodea);
    //     let inode1 = SocketInode::new(inodeb);

    //     return Ok((inode0, inode1));
    // }

    /// Actually just insert the inode into the map sothat it can be found later.
    pub fn do_bind(&self, endpoint: UnixEndpoint) -> Result<(), SystemError> {
        SEQ_MAP.try_insert(endpoint, self.self_ref.upgrade().ok_or(SystemError::EINVAL)?)
    }

    pub fn do_connect(&self, endpoint: UnixEndpoint) -> Result<(), SystemError> {
        match *self.inner.lock() {
            Status::Init | Status::Bound => todo!(),
            Status::Listen(_) | Status::Connected(_) => {
                log::error!("the socket is already bound or connected");
                return Err(SystemError::EINVAL);
            },
        }
    }

    fn try_accept(&self) -> Result<(Arc<dyn Socket>, Endpoint), SystemError> {
        match &*self.inner.read() {
            inner::Status::Listen(listen) => listen.try_accept() as _,
            _ => {
                log::error!("the socket is not listening");
                return Err(SystemError::EINVAL);
            }
        }
    }

    // fn is_acceptable(&self) -> bool {
    //     match &*self.inner.read() {
    //         inner::Status::Listen(listen) => listen.is_acceptable(),
    //         _ => {
    //             panic!("the socket is not listening");
    //         }
    //     }
    // }

    // fn is_peer_shutdown(&self) -> Result<bool, SystemError> {
    //     let peer_shutdown = match self.get_peer_name()? {
    //         Endpoint::Inode((inode, _)) => Arc::downcast::<SeqpacketSocket>(inode.inner())
    //             .map_err(|_| SystemError::EINVAL)?
    //             .shutdown
    //             .get()
    //             .is_both_shutdown(),
    //         _ => return Err(SystemError::EINVAL),
    //     };
    //     Ok(peer_shutdown)
    // }

    // fn can_recv(&self) -> Result<bool, SystemError> {
    //     let can = match &*self.inner.read() {
    //         inner::Status::Connected(connected) => connected.can_recv(),
    //         _ => return Err(SystemError::ENOTCONN),
    //     };
    //     Ok(can)
    // }

    // fn is_nonblocking(&self) -> bool {
    //     self.is_nonblocking.load(Ordering::Relaxed)
    // }

    // #[allow(dead_code)]
    // fn set_nonblocking(&self, nonblocking: bool) {
    //     self.is_nonblocking.store(nonblocking, Ordering::Relaxed);
    // }
}

impl Socket for SeqpacketSocket {

    fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        match endpoint {
            Endpoint::Unix(unix) => self.do_bind(unix),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        let endpoint = match endpoint {
            Endpoint::Unix(unix) => unix,
            _ => return Err(SystemError::EINVAL),
        };

        self.do_connect(endpoint)
    }

    fn shutdown(&self, how: ShutdownBit) -> Result<(), SystemError> {
        log::debug!("seqpacket shutdown");
        match &*self.inner.write() {
            inner::Status::Connected(connected) => connected.shutdown(how),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        let mut state = self.inner.write();
        log::debug!("listen into socket");
        let epoint = match &*state {
            inner::Status::Init(init) => init.endpoint().ok_or(SystemError::EINVAL)?.clone(),
            inner::Status::Listen(listener) => return listener.listen(backlog),
            inner::Status::Connected(_) => {
                log::error!("the socket is connected");
                return Err(SystemError::EINVAL);
            }
        };

        let listener = inner::Listener::new(epoint, backlog);
        *state = inner::Status::Listen(listener);

        Ok(())
    }

    fn accept(&self) -> Result<(Arc<dyn Socket>, Endpoint), SystemError> {
        if !self.is_nonblocking() {
            loop {
                wq_wait_event_interruptible!(self.wait_queue, self.is_acceptable(), {})?;
                match self.try_accept() {
                    Ok((socket, epoint)) => return Ok((socket, epoint)),
                    Err(_) => continue,
                }
            }
        } else {
            // ***非阻塞状态
            todo!()
        }
    }

    fn set_option(
        &self,
        _level: crate::net::socket::PSOL,
        _optname: usize,
        _optval: &[u8],
    ) -> Result<(), SystemError> {
        log::warn!("setsockopt is not implemented");
        Ok(())
    }

    fn wait_queue(&self) -> &WaitQueue {
        return &self.wait_queue;
    }

    fn close(&self) -> Result<(), SystemError> {
        // log::debug!("seqpacket close");
        self.shutdown.recv_shutdown();
        self.shutdown.send_shutdown();

        let endpoint = self.get_name()?;
        let path = match &endpoint {
            Endpoint::Inode((_, path)) => path,
            Endpoint::Unixpath((_, path)) => path,
            Endpoint::Abspath((_, path)) => path,
            _ => return Err(SystemError::EINVAL),
        };

        if path.is_empty() {
            return Ok(());
        }

        match &endpoint {
            Endpoint::Unixpath((inode_id, _)) => {
                let mut inode_guard = INODE_MAP.write_irqsave();
                inode_guard.remove(inode_id);
            }
            Endpoint::Inode((current_inode, current_path)) => {
                let mut inode_guard = INODE_MAP.write_irqsave();
                // 遍历查找匹配的条目
                let target_entry = inode_guard
                    .iter()
                    .find(|(_, ep)| {
                        if let Endpoint::Inode((map_inode, map_path)) = ep {
                            // 通过指针相等性比较确保是同一对象
                            Arc::ptr_eq(map_inode, current_inode) && map_path == current_path
                        } else {
                            log::debug!("not match");
                            false
                        }
                    })
                    .map(|(id, _)| *id);

                if let Some(id) = target_entry {
                    inode_guard.remove(&id).ok_or(SystemError::EINVAL)?;
                }
            }
            Endpoint::Abspath((abshandle, _)) => {
                let mut abs_inode_map = ABS_INODE_MAP.lock_irqsave();
                abs_inode_map.remove(&abshandle.name());
            }
            _ => {
                log::error!("invalid endpoint type");
                return Err(SystemError::EINVAL);
            }
        }

        *self.inner.write() = inner::Status::Init(inner::Init::new());
        self.wait_queue.wakeup(None);

        let _ = remove_abs_addr(path);

        return Ok(());
    }

    fn get_peer_name(&self) -> Result<Endpoint, SystemError> {
        // 获取对端地址
        let endpoint = match &*self.inner.read() {
            inner::Status::Connected(connected) => connected.peer_endpoint().cloned(),
            _ => return Err(SystemError::ENOTCONN),
        };

        if let Some(endpoint) = endpoint {
            return Ok(endpoint);
        } else {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
    }

    fn get_name(&self) -> Result<Endpoint, SystemError> {
        // 获取本端地址
        let endpoint = match &*self.inner.read() {
            inner::Status::Init(init) => init.endpoint().cloned(),
            inner::Status::Listen(listener) => Some(listener.endpoint().clone()),
            inner::Status::Connected(connected) => connected.endpoint().cloned(),
        };

        if let Some(endpoint) = endpoint {
            return Ok(endpoint);
        } else {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
    }

    fn get_option(
        &self,
        _level: crate::net::socket::PSOL,
        _name: usize,
        _value: &mut [u8],
    ) -> Result<usize, SystemError> {
        log::warn!("getsockopt is not implemented");
        Ok(0)
    }

    fn read(&self, buffer: &mut [u8]) -> Result<usize, SystemError> {
        self.recv(buffer, crate::net::socket::PMSG::empty())
    }

    fn recv(
        &self,
        buffer: &mut [u8],
        flags: crate::net::socket::PMSG,
    ) -> Result<usize, SystemError> {
        if flags.contains(PMSG::OOB) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if !flags.contains(PMSG::DONTWAIT) {
            loop {
                wq_wait_event_interruptible!(
                    self.wait_queue,
                    self.can_recv()? || self.is_peer_shutdown()?,
                    {}
                )?;
                // connect锁和flag判断顺序不正确，应该先判断在
                match &*self.inner.write() {
                    inner::Status::Connected(connected) => match connected.try_read(buffer) {
                        Ok(usize) => {
                            log::debug!("recv from successfully");
                            return Ok(usize);
                        }
                        Err(_) => continue,
                    },
                    _ => {
                        log::error!("the socket is not connected");
                        return Err(SystemError::ENOTCONN);
                    }
                }
            }
        } else {
            unimplemented!("unimplemented non_block")
        }
    }

    fn recv_msg(
        &self,
        _msg: &mut MsgHdr,
        _flags: crate::net::socket::PMSG,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn send(&self, buffer: &[u8], flags: crate::net::socket::PMSG) -> Result<usize, SystemError> {
        if flags.contains(PMSG::OOB) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if self.is_peer_shutdown()? {
            return Err(SystemError::EPIPE);
        }
        if !flags.contains(PMSG::DONTWAIT) {
            loop {
                match &*self.inner.write() {
                    inner::Status::Connected(connected) => match connected.try_write(buffer) {
                        Ok(usize) => {
                            log::debug!("send successfully");
                            return Ok(usize);
                        }
                        Err(_) => continue,
                    },
                    _ => {
                        log::error!("the socket is not connected");
                        return Err(SystemError::ENOTCONN);
                    }
                }
            }
        } else {
            unimplemented!("unimplemented non_block")
        }
    }

    fn send_msg(
        &self,
        _msg: &MsgHdr,
        _flags: crate::net::socket::PMSG,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn write(&self, buffer: &[u8]) -> Result<usize, SystemError> {
        self.send(buffer, crate::net::socket::PMSG::empty())
    }

    fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: PMSG,
        _address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        // log::debug!("recvfrom flags {:?}", flags);
        if flags.contains(PMSG::OOB) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if !flags.contains(PMSG::DONTWAIT) {
            loop {
                wq_wait_event_interruptible!(
                    self.wait_queue,
                    self.can_recv()? || self.is_peer_shutdown()?,
                    {}
                )?;
                // connect锁和flag判断顺序不正确，应该先判断在
                match &*self.inner.write() {
                    inner::Status::Connected(connected) => match connected.recv_slice(buffer) {
                        Ok(usize) => {
                            // log::debug!("recvs from successfully");
                            return Ok((usize, connected.peer_endpoint().unwrap().clone()));
                        }
                        Err(_) => continue,
                    },
                    _ => {
                        log::error!("the socket is not connected");
                        return Err(SystemError::ENOTCONN);
                    }
                }
            }
        } else {
            unimplemented!("unimplemented non_block")
        }
        //Err(SystemError::ENOSYS)
    }

    fn send_buffer_size(&self) -> usize {
        // log::warn!("using default buffer size");
        SeqpacketSocket::DEFAULT_BUF_SIZE
    }

    fn recv_buffer_size(&self) -> usize {
        // log::warn!("using default buffer size");
        SeqpacketSocket::DEFAULT_BUF_SIZE
    }

    fn poll(&self) -> usize {
        let mut mask = EP::empty();
        let shutdown = self.shutdown.get();

        // 参考linux的unix_poll https://code.dragonos.org.cn/xref/linux-6.1.9/net/unix/af_unix.c#3152
        // 用关闭读写端表示连接断开
        if shutdown.is_both_shutdown() || self.is_peer_shutdown().unwrap() {
            mask |= EP::EPOLLHUP;
        }

        if shutdown.is_recv_shutdown() {
            mask |= EP::EPOLLRDHUP | EP::EPOLLIN | EP::EPOLLRDNORM;
        }
        match &*self.inner.read() {
            inner::Status::Connected(connected) => {
                if connected.can_recv() {
                    mask |= EP::EPOLLIN | EP::EPOLLRDNORM;
                }
                // if (sk_is_readable(sk))
                // mask |= EPOLLIN | EPOLLRDNORM;

                // TODO:处理紧急情况 EPOLLPRI
                // TODO:处理连接是否关闭 EPOLLHUP
                if !shutdown.is_send_shutdown() {
                    if connected.can_send().unwrap() {
                        mask |= EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND;
                    } else {
                        todo!("poll: buffer space not enough");
                    }
                }
            }
            inner::Status::Listen(_) => mask |= EP::EPOLLIN,
            inner::Status::Init => mask |= EP::EPOLLOUT,
        }
        mask.bits() as usize
    }
}

impl IndexNode for SeqpacketSocket {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        todo!()
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        todo!()
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        unimplemented!("SeqpacketSocket fs is not implemented")
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<alloc::vec::Vec<String>, SystemError> {
        unimplemented!("SeqpacketSocket list is not implemented")
    }
}

impl PollableInode for SeqpacketSocket {
    fn poll(&self, private_data: &crate::filesystem::vfs::FilePrivateData) -> Result<usize, SystemError> {
        todo!()
    }

    fn add_epitem(
        &self,
        epitem: Arc<crate::filesystem::epoll::EPollItem>,
        private_data: &crate::filesystem::vfs::FilePrivateData,
    ) -> Result<(), SystemError> {
        todo!()
    }

    fn remove_epitem(
        &self,
        epitm: &Arc<crate::filesystem::epoll::EPollItem>,
        private_data: &crate::filesystem::vfs::FilePrivateData,
    ) -> Result<(), SystemError> {
        todo!()
    }
}
