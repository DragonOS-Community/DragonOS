pub mod inner;
use alloc::{
    string::String,
    sync::{Arc, Weak},
};
use core::sync::atomic::{AtomicBool, Ordering};
use unix::ns::abs::{remove_abs_addr, ABS_INODE_MAP};

use crate::sched::SchedMode;
use crate::{libs::rwlock::RwLock, net::socket::*};
use inner::*;
use system_error::SystemError;

use super::INODE_MAP;

type EP = EPollEventType;
#[derive(Debug)]
pub struct SeqpacketSocket {
    inner: RwLock<Inner>,
    shutdown: Shutdown,
    is_nonblocking: AtomicBool,
    wait_queue: WaitQueue,
    self_ref: Weak<Self>,
}

impl SeqpacketSocket {
    /// 默认的元数据缓冲区大小
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的缓冲区大小
    pub const DEFAULT_BUF_SIZE: usize = 64 * 1024;

    pub fn new(is_nonblocking: bool) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            inner: RwLock::new(Inner::Init(Init::new())),
            shutdown: Shutdown::new(),
            is_nonblocking: AtomicBool::new(is_nonblocking),
            wait_queue: WaitQueue::default(),
            self_ref: me.clone(),
        })
    }

    pub fn new_inode(is_nonblocking: bool) -> Result<Arc<Inode>, SystemError> {
        let socket = SeqpacketSocket::new(is_nonblocking);
        let inode = Inode::new(socket.clone());
        // 建立时绑定自身为后续能正常获取本端地址
        let _ = match &mut *socket.inner.write() {
            Inner::Init(init) => init.bind(Endpoint::Inode((inode.clone(), String::from("")))),
            _ => return Err(SystemError::EINVAL),
        };
        return Ok(inode);
    }

    pub fn new_connected(connected: Connected, is_nonblocking: bool) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            inner: RwLock::new(Inner::Connected(connected)),
            shutdown: Shutdown::new(),
            is_nonblocking: AtomicBool::new(is_nonblocking),
            wait_queue: WaitQueue::default(),
            self_ref: me.clone(),
        })
    }

    pub fn new_pairs() -> Result<(Arc<Inode>, Arc<Inode>), SystemError> {
        let socket0 = SeqpacketSocket::new(false);
        let socket1 = SeqpacketSocket::new(false);
        let inode0 = Inode::new(socket0.clone());
        let inode1 = Inode::new(socket1.clone());

        let (conn_0, conn_1) = Connected::new_pair(
            Some(Endpoint::Inode((inode0.clone(), String::from("")))),
            Some(Endpoint::Inode((inode1.clone(), String::from("")))),
        );
        *socket0.inner.write() = Inner::Connected(conn_0);
        *socket1.inner.write() = Inner::Connected(conn_1);

        return Ok((inode0, inode1));
    }

    fn try_accept(&self) -> Result<(Arc<Inode>, Endpoint), SystemError> {
        match &*self.inner.read() {
            Inner::Listen(listen) => listen.try_accept() as _,
            _ => {
                log::error!("the socket is not listening");
                return Err(SystemError::EINVAL);
            }
        }
    }

    fn is_acceptable(&self) -> bool {
        match &*self.inner.read() {
            Inner::Listen(listen) => listen.is_acceptable(),
            _ => {
                panic!("the socket is not listening");
            }
        }
    }

    fn is_peer_shutdown(&self) -> Result<bool, SystemError> {
        let peer_shutdown = match self.get_peer_name()? {
            Endpoint::Inode((inode, _)) => Arc::downcast::<SeqpacketSocket>(inode.inner())
                .map_err(|_| SystemError::EINVAL)?
                .shutdown
                .get()
                .is_both_shutdown(),
            _ => return Err(SystemError::EINVAL),
        };
        Ok(peer_shutdown)
    }

    fn can_recv(&self) -> Result<bool, SystemError> {
        let can = match &*self.inner.read() {
            Inner::Connected(connected) => connected.can_recv(),
            _ => return Err(SystemError::ENOTCONN),
        };
        Ok(can)
    }

    fn is_nonblocking(&self) -> bool {
        self.is_nonblocking.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    fn set_nonblocking(&self, nonblocking: bool) {
        self.is_nonblocking.store(nonblocking, Ordering::Relaxed);
    }
}

impl Socket for SeqpacketSocket {
    fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        let peer_inode = match endpoint {
            Endpoint::Inode((inode, _)) => inode,
            Endpoint::Unixpath((inode_id, _)) => {
                let inode_guard = INODE_MAP.read_irqsave();
                let inode = inode_guard.get(&inode_id).unwrap();
                match inode {
                    Endpoint::Inode((inode, _)) => inode.clone(),
                    _ => return Err(SystemError::EINVAL),
                }
            }
            Endpoint::Abspath((abs_addr, _)) => {
                let inode_guard = ABS_INODE_MAP.lock_irqsave();
                let inode = match inode_guard.get(&abs_addr.name()) {
                    Some(inode) => inode,
                    None => {
                        log::debug!("can not find inode from absInodeMap");
                        return Err(SystemError::EINVAL);
                    }
                };
                match inode {
                    Endpoint::Inode((inode, _)) => inode.clone(),
                    _ => {
                        log::debug!("when connect, find inode failed!");
                        return Err(SystemError::EINVAL);
                    }
                }
            }
            _ => return Err(SystemError::EINVAL),
        };
        // 远端为服务端
        let remote_socket = Arc::downcast::<SeqpacketSocket>(peer_inode.inner())
            .map_err(|_| SystemError::EINVAL)?;

        let client_epoint = match &mut *self.inner.write() {
            Inner::Init(init) => match init.endpoint().cloned() {
                Some(end) => {
                    log::debug!("bind when connect");
                    Some(end)
                }
                None => {
                    log::debug!("not bind when connect");
                    let inode = Inode::new(self.self_ref.upgrade().unwrap().clone());
                    let epoint = Endpoint::Inode((inode.clone(), String::from("")));
                    let _ = init.bind(epoint.clone());
                    Some(epoint)
                }
            },
            Inner::Listen(_) => return Err(SystemError::EINVAL),
            Inner::Connected(_) => return Err(SystemError::EISCONN),
        };
        // ***阻塞与非阻塞处理还未实现
        // 客户端与服务端建立连接将服务端inode推入到自身的listen_incom队列中，
        // accept时从中获取推出对应的socket
        match &*remote_socket.inner.read() {
            Inner::Listen(listener) => match listener.push_incoming(client_epoint) {
                Ok(connected) => {
                    *self.inner.write() = Inner::Connected(connected);
                    log::debug!("try to wake up");

                    remote_socket.wait_queue.wakeup(None);
                    return Ok(());
                }
                // ***错误处理
                Err(_) => todo!(),
            },
            Inner::Init(_) => {
                log::debug!("init einval");
                return Err(SystemError::EINVAL);
            }
            Inner::Connected(_) => return Err(SystemError::EISCONN),
        };
    }

    fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        // 将自身socket的inode与用户端提供路径的文件indoe_id进行绑定
        match endpoint {
            Endpoint::Unixpath((inodeid, path)) => {
                let inode = match &mut *self.inner.write() {
                    Inner::Init(init) => init.bind_path(path)?,
                    _ => {
                        log::error!("socket has listen or connected");
                        return Err(SystemError::EINVAL);
                    }
                };

                INODE_MAP.write_irqsave().insert(inodeid, inode);
                Ok(())
            }
            Endpoint::Abspath((abshandle, path)) => {
                let inode = match &mut *self.inner.write() {
                    Inner::Init(init) => init.bind_path(path)?,
                    _ => {
                        log::error!("socket has listen or connected");
                        return Err(SystemError::EINVAL);
                    }
                };
                ABS_INODE_MAP.lock_irqsave().insert(abshandle.name(), inode);
                Ok(())
            }
            _ => return Err(SystemError::EINVAL),
        }
    }

    fn shutdown(&self, how: ShutdownTemp) -> Result<(), SystemError> {
        log::debug!("seqpacket shutdown");
        match &*self.inner.write() {
            Inner::Connected(connected) => connected.shutdown(how),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        let mut state = self.inner.write();
        log::debug!("listen into socket");
        let epoint = match &*state {
            Inner::Init(init) => init.endpoint().ok_or(SystemError::EINVAL)?.clone(),
            Inner::Listen(listener) => return listener.listen(backlog),
            Inner::Connected(_) => {
                log::error!("the socket is connected");
                return Err(SystemError::EINVAL);
            }
        };

        let listener = Listener::new(epoint, backlog);
        *state = Inner::Listen(listener);

        Ok(())
    }

    fn accept(&self) -> Result<(Arc<Inode>, Endpoint), SystemError> {
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

        *self.inner.write() = Inner::Init(Init::new());
        self.wait_queue.wakeup(None);

        let _ = remove_abs_addr(path);

        return Ok(());
    }

    fn get_peer_name(&self) -> Result<Endpoint, SystemError> {
        // 获取对端地址
        let endpoint = match &*self.inner.read() {
            Inner::Connected(connected) => connected.peer_endpoint().cloned(),
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
            Inner::Init(init) => init.endpoint().cloned(),
            Inner::Listen(listener) => Some(listener.endpoint().clone()),
            Inner::Connected(connected) => connected.endpoint().cloned(),
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
                    Inner::Connected(connected) => match connected.try_read(buffer) {
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
        _msg: &mut crate::net::syscall::MsgHdr,
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
                    Inner::Connected(connected) => match connected.try_write(buffer) {
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
        _msg: &crate::net::syscall::MsgHdr,
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
                    Inner::Connected(connected) => match connected.recv_slice(buffer) {
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
            Inner::Connected(connected) => {
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
            Inner::Listen(_) => mask |= EP::EPOLLIN,
            Inner::Init(_) => mask |= EP::EPOLLOUT,
        }
        mask.bits() as usize
    }
}
