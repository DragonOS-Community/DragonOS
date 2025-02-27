use crate::sched::SchedMode;
use alloc::{
    string::String,
    sync::{Arc, Weak},
};
use inner::{Connected, Init, Inner, Listener};
use log::debug;
use system_error::SystemError;
use unix::{
    ns::abs::{remove_abs_addr, ABS_INODE_MAP},
    INODE_MAP,
};

use crate::{
    libs::rwlock::RwLock,
    net::socket::{self, *},
};

type EP = EPollEventType;

pub mod inner;

#[derive(Debug)]
pub struct StreamSocket {
    inner: RwLock<Inner>,
    shutdown: Shutdown,
    _epitems: EPollItems,
    wait_queue: WaitQueue,
    self_ref: Weak<Self>,
}

impl StreamSocket {
    /// 默认的元数据缓冲区大小
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的缓冲区大小
    pub const DEFAULT_BUF_SIZE: usize = 64 * 1024;

    pub fn new() -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            inner: RwLock::new(Inner::Init(Init::new())),
            shutdown: Shutdown::new(),
            _epitems: EPollItems::default(),
            wait_queue: WaitQueue::default(),
            self_ref: me.clone(),
        })
    }

    pub fn new_pairs() -> Result<(Arc<Inode>, Arc<Inode>), SystemError> {
        let socket0 = StreamSocket::new();
        let socket1 = StreamSocket::new();
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

    pub fn new_connected(connected: Connected) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            inner: RwLock::new(Inner::Connected(connected)),
            shutdown: Shutdown::new(),
            _epitems: EPollItems::default(),
            wait_queue: WaitQueue::default(),
            self_ref: me.clone(),
        })
    }

    pub fn new_inode() -> Result<Arc<Inode>, SystemError> {
        let socket = StreamSocket::new();
        let inode = Inode::new(socket.clone());

        let _ = match &mut *socket.inner.write() {
            Inner::Init(init) => init.bind(Endpoint::Inode((inode.clone(), String::from("")))),
            _ => return Err(SystemError::EINVAL),
        };

        return Ok(inode);
    }

    fn is_acceptable(&self) -> bool {
        match &*self.inner.read() {
            Inner::Listener(listener) => listener.is_acceptable(),
            _ => {
                panic!("the socket is not listening");
            }
        }
    }

    pub fn try_accept(&self) -> Result<(Arc<Inode>, Endpoint), SystemError> {
        match &*self.inner.read() {
            Inner::Listener(listener) => listener.try_accept() as _,
            _ => {
                log::error!("the socket is not listening");
                return Err(SystemError::EINVAL);
            }
        }
    }

    fn is_peer_shutdown(&self) -> Result<bool, SystemError> {
        let peer_shutdown = match self.get_peer_name()? {
            Endpoint::Inode((inode, _)) => Arc::downcast::<StreamSocket>(inode.inner())
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
}

impl Socket for StreamSocket {
    fn connect(&self, server_endpoint: Endpoint) -> Result<(), SystemError> {
        //获取客户端地址
        let client_endpoint = match &mut *self.inner.write() {
            Inner::Init(init) => match init.endpoint().cloned() {
                Some(endpoint) => {
                    debug!("bind when connected");
                    Some(endpoint)
                }
                None => {
                    debug!("not bind when connected");
                    let inode = Inode::new(self.self_ref.upgrade().unwrap().clone());
                    let epoint = Endpoint::Inode((inode.clone(), String::from("")));
                    let _ = init.bind(epoint.clone());
                    Some(epoint)
                }
            },
            Inner::Connected(_) => return Err(SystemError::EISCONN),
            Inner::Listener(_) => return Err(SystemError::EINVAL),
        };
        //获取服务端地址
        // let peer_inode = match server_endpoint.clone() {
        //     Endpoint::Inode(socket) => socket,
        //     _ => return Err(SystemError::EINVAL),
        // };

        //找到对端socket
        let (peer_inode, sun_path) = match server_endpoint {
            Endpoint::Inode((inode, path)) => (inode, path),
            Endpoint::Unixpath((inode_id, path)) => {
                let inode_guard = INODE_MAP.read_irqsave();
                let inode = inode_guard.get(&inode_id).unwrap();
                match inode {
                    Endpoint::Inode((inode, _)) => (inode.clone(), path),
                    _ => return Err(SystemError::EINVAL),
                }
            }
            Endpoint::Abspath((abs_addr, path)) => {
                let inode_guard = ABS_INODE_MAP.lock_irqsave();
                let inode = match inode_guard.get(&abs_addr.name()) {
                    Some(inode) => inode,
                    None => {
                        log::debug!("can not find inode from absInodeMap");
                        return Err(SystemError::EINVAL);
                    }
                };
                match inode {
                    Endpoint::Inode((inode, _)) => (inode.clone(), path),
                    _ => {
                        debug!("when connect, find inode failed!");
                        return Err(SystemError::EINVAL);
                    }
                }
            }
            _ => return Err(SystemError::EINVAL),
        };

        let remote_socket: Arc<StreamSocket> =
            Arc::downcast::<StreamSocket>(peer_inode.inner()).map_err(|_| SystemError::EINVAL)?;

        //创建新的对端socket
        let new_server_socket = StreamSocket::new();
        let new_server_inode = Inode::new(new_server_socket.clone());
        let new_server_endpoint = Some(Endpoint::Inode((new_server_inode.clone(), sun_path)));
        //获取connect pair
        let (client_conn, server_conn) =
            Connected::new_pair(client_endpoint, new_server_endpoint.clone());
        *new_server_socket.inner.write() = Inner::Connected(server_conn);

        //查看remote_socket是否处于监听状态
        let remote_listener = remote_socket.inner.write();
        match &*remote_listener {
            Inner::Listener(listener) => {
                //往服务端socket的连接队列中添加connected
                listener.push_incoming(new_server_inode)?;
                *self.inner.write() = Inner::Connected(client_conn);
                remote_socket.wait_queue.wakeup(None);
            }
            _ => return Err(SystemError::EINVAL),
        }

        return Ok(());
    }

    fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError> {
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

    fn shutdown(&self, _stype: ShutdownTemp) -> Result<(), SystemError> {
        todo!();
    }

    fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        let mut inner = self.inner.write();
        let epoint = match &*inner {
            Inner::Init(init) => init.endpoint().ok_or(SystemError::EINVAL)?.clone(),
            Inner::Connected(_) => {
                return Err(SystemError::EINVAL);
            }
            Inner::Listener(listener) => {
                return listener.listen(backlog);
            }
        };

        let listener = Listener::new(Some(epoint), backlog);
        *inner = Inner::Listener(listener);

        return Ok(());
    }

    fn accept(&self) -> Result<(Arc<socket::Inode>, Endpoint), SystemError> {
        debug!("stream server begin accept");
        //目前只实现了阻塞式实现
        loop {
            wq_wait_event_interruptible!(self.wait_queue, self.is_acceptable(), {})?;
            match self.try_accept() {
                Ok((socket, endpoint)) => {
                    debug!("server accept!:{:?}", endpoint);
                    return Ok((socket, endpoint));
                }
                Err(_) => continue,
            }
        }
    }

    fn set_option(&self, _level: PSOL, _optname: usize, _optval: &[u8]) -> Result<(), SystemError> {
        log::warn!("setsockopt is not implemented");
        Ok(())
    }

    fn wait_queue(&self) -> &WaitQueue {
        return &self.wait_queue;
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
            Inner::Listener(_) => mask |= EP::EPOLLIN,
            Inner::Init(_) => mask |= EP::EPOLLOUT,
        }
        mask.bits() as usize
    }

    fn close(&self) -> Result<(), SystemError> {
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

        Ok(())
    }

    fn get_peer_name(&self) -> Result<Endpoint, SystemError> {
        //获取对端地址
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
        //获取本端地址
        let endpoint = match &*self.inner.read() {
            Inner::Init(init) => init.endpoint().cloned(),
            Inner::Connected(connected) => connected.endpoint().cloned(),
            Inner::Listener(listener) => listener.endpoint().cloned(),
        };

        if let Some(endpoint) = endpoint {
            return Ok(endpoint);
        } else {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
    }

    fn get_option(
        &self,
        _level: PSOL,
        _name: usize,
        _value: &mut [u8],
    ) -> Result<usize, SystemError> {
        log::warn!("getsockopt is not implemented");
        Ok(0)
    }

    fn read(&self, buffer: &mut [u8]) -> Result<usize, SystemError> {
        self.recv(buffer, socket::PMSG::empty())
    }

    fn recv(&self, buffer: &mut [u8], flags: socket::PMSG) -> Result<usize, SystemError> {
        if !flags.contains(PMSG::DONTWAIT) {
            loop {
                log::debug!("socket try recv");
                wq_wait_event_interruptible!(
                    self.wait_queue,
                    self.can_recv()? || self.is_peer_shutdown()?,
                    {}
                )?;
                // connect锁和flag判断顺序不正确，应该先判断在
                match &*self.inner.write() {
                    Inner::Connected(connected) => match connected.try_recv(buffer) {
                        Ok(usize) => {
                            log::debug!("recv successfully");
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

    fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: socket::PMSG,
        _address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        if flags.contains(PMSG::OOB) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if !flags.contains(PMSG::DONTWAIT) {
            loop {
                log::debug!("socket try recv from");

                wq_wait_event_interruptible!(
                    self.wait_queue,
                    self.can_recv()? || self.is_peer_shutdown()?,
                    {}
                )?;
                // connect锁和flag判断顺序不正确，应该先判断在
                log::debug!("try recv");

                match &*self.inner.write() {
                    Inner::Connected(connected) => match connected.try_recv(buffer) {
                        Ok(usize) => {
                            log::debug!("recvs from successfully");
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
    }

    fn recv_msg(
        &self,
        _msg: &mut crate::net::syscall::MsgHdr,
        _flags: socket::PMSG,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn send(&self, buffer: &[u8], flags: socket::PMSG) -> Result<usize, SystemError> {
        if self.is_peer_shutdown()? {
            return Err(SystemError::EPIPE);
        }
        if !flags.contains(PMSG::DONTWAIT) {
            loop {
                match &*self.inner.write() {
                    Inner::Connected(connected) => match connected.try_send(buffer) {
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
        _flags: socket::PMSG,
    ) -> Result<usize, SystemError> {
        todo!()
    }

    fn send_to(
        &self,
        _buffer: &[u8],
        _flags: socket::PMSG,
        _address: Endpoint,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn write(&self, buffer: &[u8]) -> Result<usize, SystemError> {
        self.send(buffer, socket::PMSG::empty())
    }

    fn send_buffer_size(&self) -> usize {
        log::warn!("using default buffer size");
        StreamSocket::DEFAULT_BUF_SIZE
    }

    fn recv_buffer_size(&self) -> usize {
        log::warn!("using default buffer size");
        StreamSocket::DEFAULT_BUF_SIZE
    }
}
