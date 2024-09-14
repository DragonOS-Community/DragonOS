use alloc::sync::{Arc, Weak};
use inner::{Connected, Init, Inner, Listener};
use system_error::SystemError;

use crate::{
    libs::rwlock::RwLock,
    net::socket::{
        self,
        common::{
            poll_unit::{EPollItems, WaitQueue},
            Shutdown,
        },
        Endpoint, Inode, MessageFlag, OptionsLevel, ShutdownTemp, Socket,
    },
};

pub mod inner;

#[derive(Debug)]
pub struct StreamSocket {           
    inner: RwLock<Inner>,
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
            inner: RwLock::new(Inner::Init(Init::new())),
            shutdown: Shutdown::new(),
            epitems: EPollItems::default(),
            wait_queue: WaitQueue::default(),
            self_ref: me.clone(),
        })
    }

    pub fn new_connected(connected: Connected) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            inner: RwLock::new(Inner::Connected(connected)),
            shutdown: Shutdown::new(),
            epitems: EPollItems::default(),
            wait_queue: WaitQueue::default(),
            self_ref: me.clone(),
        })
    }

    pub fn new_pair() -> (Arc<Self>, Arc<Self>) {
        let (conn, peer_conn) = Connected::new_pair(None, None);
        (
            StreamSocket::new_connected(conn),
            StreamSocket::new_connected(peer_conn),
        )
    }

    pub fn do_bind(&self, local_endpoint: Endpoint) -> Result<(), SystemError> {

        match &mut *self.inner.write() {
            Inner::Init(inner) => {
                inner.bind(local_endpoint)?;
                Ok(())
            }
            _ => Err(SystemError::EINVAL),
        }
    }

    pub fn do_listen(&self, backlog: usize) -> Result<(), SystemError> {
        let mut inner = self.inner.write();
        match & *inner {
            Inner::Init(init) => {
                let listener = Listener::new(init.addr(), backlog);
                *inner = Inner::Listener(listener);
            }
            Inner::Connected(_) => {
                return Err(SystemError::EINVAL);
            }
            Inner::Listener(listener) => {
                return listener.listen(backlog);
            }
        };
        return Ok(());
    }
}


impl Socket for StreamSocket {
    fn connect(&self, server_endpoint: Endpoint) -> Result<(), SystemError> {
        //获取客户端地址
        let inner = self.inner.read();
        let client_endpoint: Option<Endpoint> = match &*inner {
            Inner::Init(socket) => socket.addr().clone(),
            Inner::Connected(_) => return Err(SystemError::EINVAL),
            Inner::Listener(_) => return Err(SystemError::EINVAL),
        };
        drop(inner);
        
        //获取服务端地址
        let peer_inode = match server_endpoint.clone() {
            Endpoint::Inode(socket) => socket,
            _ => return Err(SystemError::EINVAL),
        };

        //创建新的对端endpoint
        let server_inode = StreamSocket::new();
        let new_server_endpoint = Some(Endpoint::Inode(Inode::new(server_inode.clone())));
        //获取connect pair
        let (client_conn, server_conn) = Connected::new_pair(client_endpoint, new_server_endpoint);

        *server_inode.inner.write() = Inner::Connected(server_conn);

        let remote_socket: Arc<StreamSocket> =
            Arc::downcast::<StreamSocket>(peer_inode.inner()).map_err(|_| SystemError::EINVAL)?;

        //查看remote_socket是否处于监听状态
        let remote_listener = remote_socket.inner.write();
        match & *remote_listener {
            Inner::Listener(listener) => {
                //往服务端socket的连接队列中添加connected
                listener.push_incoming(Inode::new(server_inode as Arc<dyn Socket>))?;
            }
            _ => return Err(SystemError::EINVAL),
        }

        //更新客户端状态进入连接
        let mut inner = self.inner.write();
        *inner = Inner::Connected(client_conn);
        
        return Ok(());
    }

    fn bind(&self, _endpoint: Endpoint) -> Result<(), SystemError> {
        return self.do_bind(_endpoint);
    }

    fn shutdown(&self, _stype: ShutdownTemp) -> Result<(), SystemError> {
        todo!();
    }

    fn listen(&self, _backlog: usize) -> Result<(), SystemError> {
        return self.do_listen(_backlog);
    }

    fn accept(&self) -> Result<(Arc<socket::Inode>, Endpoint), SystemError> {
        match & *self.inner.read() {
            Inner::Listener(listener) => {
                let server_inode = listener.pop_incoming().unwrap();
                return Ok({(
                    server_inode.clone(),
                    Endpoint::Inode(server_inode.clone()),
                )})
            }
            _ => {
                return Err(SystemError::EINVAL);
            }
        }
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

    fn wait_queue(&self) -> WaitQueue {
        return self.wait_queue.clone();
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
        _level: OptionsLevel,
        _name: usize,
        _value: &mut [u8],
    ) -> Result<usize, SystemError> {
        log::warn!("getsockopt is not implemented");
        Ok(0)
    }

    fn read(&self, buffer: &mut [u8]) -> Result<usize, SystemError> {
        self.recv(buffer, socket::MessageFlag::empty())
    }

    fn recv(&self, buffer: &mut [u8], flags: socket::MessageFlag) -> Result<usize, SystemError> {
        let inner = self.inner.read();
        let conn = match & *inner {
            Inner::Connected(connected) => connected,
            _ => return Err(SystemError::EINVAL),
        };

        if !flags.contains(MessageFlag::DONTWAIT) {
            //阻塞式读取
            //忙询直到缓冲区有数据可以读取
            loop {
                match conn.try_recv(buffer) {
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
        flags: socket::MessageFlag,
        _address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        match & *self.inner.write() {
            Inner::Connected(connected) => {
                if flags.contains(MessageFlag::OOB) {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                if !flags.contains(MessageFlag::DONTWAIT) {
                    loop {
                        match connected.try_recv(buffer) {
                            Ok(usize) => return Ok((usize, connected.peer_addr().unwrap().clone())),
                            Err(_) => continue,
                        }
                    }
                } else {
                    unimplemented!("unimplemented non_block");
                }
            }
            _ => {
                log::error!("the socket is not connected");
                return Err(SystemError::ENOTCONN);
            }
        }
    }

    fn recv_msg(
        &self,
        _msg: &mut crate::net::syscall::MsgHdr,
        _flags: socket::MessageFlag,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn send(&self, buffer: &[u8], flags: socket::MessageFlag) -> Result<usize, SystemError> {
        let inner = self.inner.read();
        let conn = match & *inner {
            Inner::Connected(connected) => connected,
            _ => return Err(SystemError::EINVAL),
        };

        if !flags.contains(MessageFlag::DONTWAIT) {
            //阻塞式读取
            //忙询直到缓冲区有数据可以发送
            loop {
                match conn.try_send(buffer) {
                    Ok(len) => return Ok(len),
                    Err(_) => continue,
                }
            }
        } else {
            unimplemented!("not implement non_block")
        }
    }

    fn send_msg(
        &self,
        _msg: &crate::net::syscall::MsgHdr,
        _flags: socket::MessageFlag,
    ) -> Result<usize, SystemError> {
        todo!()
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
