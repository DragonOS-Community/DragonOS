use alloc::sync::{Arc, Weak};
use inner::{Connected, Init, Inner, Listener};
use intertrait::CastFromSync;
use system_error::SystemError;

use crate::{
    libs::rwlock::RwLock,
    net::socket::{self, *},
};

pub mod inner;

#[derive(Debug)]
pub struct StreamSocket {
    buffer: Arc<Buffer>,
    inner: RwLock<Option<Inner>>,
    shutdown: Shutdown,
    epitems: EPollItems,
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
            buffer: Buffer::new(),
            inner: RwLock::new(Some(Inner::Init(Init::new()))),
            shutdown: Shutdown::new(),
            epitems: EPollItems::default(),
            wait_queue: WaitQueue::default(),
            self_ref: me.clone(),
        })
    }

    pub fn new_connected(connected: Connected) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            buffer: Buffer::new(),
            inner: RwLock::new(Some(Inner::Connected(connected))),
            shutdown: Shutdown::new(),
            epitems: EPollItems::default(),
            wait_queue: WaitQueue::default(),
            self_ref: me.clone(),
        })
    }

    pub fn do_bind(&self, local_endpoint: Endpoint) -> Result<(), SystemError> {
        let mut guard = self.inner.write();
        match guard.take().expect("Unix Stream Socket is None") {
            Inner::Init(mut inner) => {
                inner.bind(local_endpoint)?;
                guard.replace(Inner::Init(inner));
                Ok(())
            }
            _ => Err(SystemError::EINVAL),
        }
    }

    pub fn do_listen(&self, backlog: usize) -> Result<(), SystemError> {
        let mut inner = self.inner.write();
        let addr = match inner.take().expect("Unix Stream Socket is None") {
            Inner::Init(init) => init.addr().unwrap(),
            Inner::Connected(_) => {
                return Err(SystemError::EINVAL);
            }
            Inner::Listener(listener) => {
                return listener.listen(backlog);
            }
        };

        let listener = Listener::new(Some(addr), backlog);
        inner.replace(Inner::Listener(listener));
        return Ok(());
    }

    pub fn do_connect(&self, remote_socket: Arc<StreamSocket>) -> Result<(), SystemError> {
        let mut client = self.inner.write();
        let client_endpoint = match client.take() {
            Some(inner) => match inner {
                Inner::Init(socket) => socket.addr().clone(),
                Inner::Connected(_) => return Err(SystemError::EINVAL),
                Inner::Listener(_) => return Err(SystemError::EINVAL),
            },
            None => return Err(SystemError::EINVAL),
        };

        //查看remote_socket是否处于监听状态
        let mut remote_inner = remote_socket.inner.write();
        match remote_inner.take().expect("unix stream sock is none") {
            Inner::Listener(listener) => {
                //往服务端socket的连接队列中添加connected
                listener.push_incoming(client_endpoint);
                remote_inner.replace(Inner::Listener(listener));
                return Ok(());
            }
            _ => return Err(SystemError::EINVAL),
        }
    }

    pub fn do_accept(&self) -> Result<(Arc<StreamSocket>, Endpoint), SystemError> {
        let mut inner = self.inner.write();
        match inner.take().expect("Unix Stream Socket is None") {
            Inner::Listener(listener) => {
                let server_conn = listener.pop_incoming();
                let peer_addr = server_conn
                    .clone()
                    .take()
                    .expect("Unix Stream Socket is none")
                    .peer_addr()
                    .unwrap();

                return Ok((StreamSocket::new_connected(server_conn.unwrap()), peer_addr));
            }
            _ => {
                return Err(SystemError::EINVAL);
            }
        }
    }

    fn send_slice(&self, buf: &[u8]) -> Result<usize, SystemError> {
        //找到peer_inode，并将write_buffer的内容写入对端的read_buffer
        let mut inner = self.inner.write();
        match inner.take().expect("Unix Stream Socket is None") {
            Inner::Connected(connected) => {
                let peer_inode = connected.peer_addr().unwrap();
                match peer_inode {
                    Endpoint::Inode(inode) => {
                        let remote_socket: Arc<StreamSocket> = Arc::clone(&inode)
                            .arc_any()
                            .downcast()
                            .map_err(|_| SystemError::EINVAL)?;
                        let usize = remote_socket.buffer.write_read_buffer(buf)?;
                        inner.replace(Inner::Connected(connected));
                        Ok(usize)
                    }
                    _ => return Err(SystemError::EINVAL),
                }
            }
            _ => return Err(SystemError::EINVAL),
        }
    }

    fn can_send(&self) -> Result<bool, SystemError> {
        //获取对端socket的read_buffer查看是否为空
        let binding = self.inner.read();
        let inner = binding.as_ref().unwrap();
        match inner {
            Inner::Connected(conn) => {
                let peer_inode = conn.peer_addr().unwrap();
                match peer_inode {
                    Endpoint::Inode(inode) => {
                        let remote_socket: Arc<StreamSocket> = Arc::clone(&inode)
                            .arc_any()
                            .downcast()
                            .map_err(|_| SystemError::EINVAL)?;
                        Ok(remote_socket.buffer.is_read_buf_empty())
                    }
                    _ => return Err(SystemError::EINVAL),
                }
            }
            _ => return Err(SystemError::EINVAL),
        }
    }

    fn can_recv(&self) -> bool {
        return !self.buffer.is_read_buf_empty();
    }

    fn try_send(&self, buf: &[u8]) -> Result<usize, SystemError> {
        if self.can_send()? {
            return self.send_slice(buf);
        } else {
            return Err(SystemError::ENOBUFS);
        }
    }

    fn recv_slice(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        return self.buffer.read_read_buffer(buf);
    }

    fn try_recv(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        if self.can_recv() {
            return self.recv_slice(buf);
        } else {
            return Err(SystemError::EINVAL);
        }
    }
}

impl Socket for StreamSocket {
    fn connect(&self, _endpoint: Endpoint) -> Result<(), SystemError> {
        //使用endpoint获取服务端socket
        let remote_socket = match _endpoint {
            Endpoint::Inode(socket) => socket.clone(),
            _ => return Err(SystemError::EINVAL),
        };

        //客户端建立connected连接
        let mut client_socket = self.inner.write();
        match client_socket.take().expect("Unix Stream Socket is None") {
            Inner::Init(inner) => {
                let remote_endpoint = Some(Endpoint::Inode(remote_socket.clone()));
                let client_conn = Connected::new(inner.addr().clone(), remote_endpoint);
                client_socket.replace(Inner::Connected(client_conn));
            }
            _ => {
                return Err(SystemError::EINVAL);
            }
        }

        let remote_stream_socket: Arc<StreamSocket> = Arc::clone(&remote_socket)
            .arc_any()
            .downcast()
            .map_err(|_| SystemError::EINVAL)?;

        //服务端建立连接
        return self.do_connect(remote_stream_socket);
    }

    fn bind(&self, _endpoint: Endpoint) -> Result<(), SystemError> {
        return self.do_bind(_endpoint);
    }

    fn shutdown(&self, stype: ShutdownTemp) -> Result<(), SystemError> {
        match self
            .inner
            .write()
            .take()
            .expect("Unix Stream Socket is None")
        {
            Inner::Connected(conn) => conn.shutdown(stype),
            _ => return Err(SystemError::EINVAL),
        }
    }

    fn listen(&self, _backlog: usize) -> Result<(), SystemError> {
        return self.do_listen(_backlog);
    }

    fn accept(&self) -> Result<(Arc<socket::Inode>, Endpoint), SystemError> {
        self.do_accept().map(|(stream, remote_endpoint)| {
            (
                Inode::new(stream as Arc<dyn Socket>),
                Endpoint::from(remote_endpoint),
            )
        })
    }

    fn set_option(
        &self,
        _level: OptionsLevel,
        _optname: usize,
        _optval: &[u8],
    ) -> Result<(), SystemError> {
        log::warn!("setsockopt is not implemented");
        Ok(())
    }

    fn wait_queue(&self) -> &WaitQueue {
        todo!()
    }

    fn poll(&self) -> usize {
        todo!()
    }

    fn close(&self) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn get_peer_name(&self) -> Result<Endpoint, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn get_name(&self) -> Result<Endpoint, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn get_option(
        &self,
        level: OptionsLevel,
        name: usize,
        value: &mut [u8],
    ) -> Result<usize, SystemError> {
        log::warn!("getsockopt is not implemented");
        Ok(0)
    }

    fn read(&self, buffer: &mut [u8]) -> Result<usize, SystemError> {
        self.recv(buffer, socket::MessageFlag::empty())
    }

    fn recv(&self, buffer: &mut [u8], flags: socket::MessageFlag) -> Result<usize, SystemError> {
        if flags == MessageFlag::DONTWAIT {
            //阻塞式读取
            //忙询直到缓冲区有数据可以读取
            loop {
                match self.try_recv(buffer) {
                    Ok(len) => return Ok(len),
                    Err(_) => continue,
                }
            }
        } else {
            unimplemented!("为实现非阻塞式处理")
        }
    }

    fn recv_from(
        &self,
        buffer: &mut [u8],
        len: usize,
        flags: socket::MessageFlag,
        address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn recv_msg(
        &self,
        msg: &mut crate::net::syscall::MsgHdr,
        flags: socket::MessageFlag,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn send(&self, buffer: &[u8], flags: socket::MessageFlag) -> Result<usize, SystemError> {
        if flags == MessageFlag::DONTWAIT {
            //阻塞式读取
            //忙询直到缓冲区有数据可以读取
            loop {
                match self.try_send(buffer) {
                    Ok(len) => return Ok(len),
                    Err(_) => continue,
                }
            }
        } else {
            unimplemented!("为实现非阻塞式处理")
        }
    }

    fn send_msg(
        &self,
        msg: &crate::net::syscall::MsgHdr,
        flags: socket::MessageFlag,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn send_to(
        &self,
        buffer: &[u8],
        flags: socket::MessageFlag,
        address: Endpoint,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn write(&self, buffer: &[u8]) -> Result<usize, SystemError> {
        self.send(buffer, socket::MessageFlag::empty())
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
