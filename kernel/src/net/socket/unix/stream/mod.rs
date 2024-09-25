use alloc::{sync::{Arc, Weak},string::String};
use inner::{Connected, Init, Inner, Listener};
use log::debug;
use system_error::SystemError;
use unix::INODE_MAP;
use crate::sched::SchedMode;

use crate::{
    libs::rwlock::RwLock, net::socket::{self, *}
};


pub mod inner;

#[derive(Debug)]
pub struct StreamSocket {           
    inner: RwLock<Inner>,
    _shutdown: Shutdown,
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
            _shutdown: Shutdown::new(),
            _epitems: EPollItems::default(),
            wait_queue: WaitQueue::default(),
            self_ref: me.clone(),
        })
    }

    pub fn new_connected(connected: Connected) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            inner: RwLock::new(Inner::Connected(connected)),
            _shutdown: Shutdown::new(),
            _epitems: EPollItems::default(),
            wait_queue: WaitQueue::default(),
            self_ref: me.clone(),
        })
    }

    pub fn new_inode() -> Result<Arc<Inode>, SystemError> {
        let socket = StreamSocket::new();
        let inode = Inode::new(socket.clone());

        let _ = match &mut *socket.inner.write() {
            Inner::Init(init) => init.bind(Endpoint::Inode((inode.clone(),String::from("")))),
            _ => return Err(SystemError::EINVAL),
        };

        return Ok(inode)
    }

    pub fn new_pairs() -> (Arc<Self>, Arc<Self>) {
        let (conn, peer_conn) = Connected::new_pair(None, None);
        (
            StreamSocket::new_connected(conn),
            StreamSocket::new_connected(peer_conn),
        )
    }

    fn is_acceptable(&self) -> bool {
        match & *self.inner.read() {
            Inner::Listener(listener) => listener.is_acceptable(),
            _ => {
                panic!("the socket is not listening");
            }
        }
    }

    pub fn try_accept(&self) -> Result<(Arc<Inode>, Endpoint), SystemError> {
        match &* self.inner.read() {
            Inner::Listener(listener) => listener.try_accept() as _,
            _ => {
                log::error!("the socket is not listening");
                return Err(SystemError::EINVAL)
            }
        }
    }

}


impl Socket for StreamSocket {
    fn connect(&self, server_endpoint: Endpoint) -> Result<(), SystemError> {
        //获取客户端地址
        let client_endpoint = match &mut *self.inner.write() {
            Inner::Init(init) => {
                match init.endpoint().cloned() {
                    Some(endpoint) => {
                        debug!("bind when connected");
                        Some(endpoint)
                    },
                    None => {
                        debug!("not bind when connected");
                        let inode = Inode::new(self.self_ref.upgrade().unwrap().clone());
                        let epoint =Endpoint::Inode((inode.clone(),String::from("")));
                        let _ = init.bind(epoint.clone());
                        Some(epoint)
                    }
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
        let mut sun_path=String::from("");
        let peer_inode = match server_endpoint {
            Endpoint::Inode((inode,path)) => {
                sun_path = path;
                inode
            },
            Endpoint::Unixpath((inode_id,path)) => {
                sun_path = path;
                let inode_guard = INODE_MAP.read_irqsave();
                let inode = inode_guard.get(&inode_id).unwrap();
                match inode {
                    Endpoint::Inode((inode,_)) => inode.clone(),
                    _ => return Err(SystemError::EINVAL),
                }
            }
            _ => return Err(SystemError::EINVAL),
        };

        let remote_socket: Arc<StreamSocket> =
        Arc::downcast::<StreamSocket>(peer_inode.inner()).map_err(|_| SystemError::EINVAL)?;

        //创建新的对端socket
        let new_server_socket = StreamSocket::new();
        let new_server_inode = Inode::new(new_server_socket.clone());
        let new_server_endpoint = Some(Endpoint::Inode((new_server_inode.clone(),sun_path)));
        //获取connect pair
        let (client_conn, server_conn) = Connected::new_pair(client_endpoint, new_server_endpoint.clone());
        *new_server_socket.inner.write() = Inner::Connected(server_conn);

        //查看remote_socket是否处于监听状态
        let remote_listener = remote_socket.inner.write();
        match & *remote_listener {
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
            Endpoint::Unixpath((inodeid,path)) => {
                let inode = match &mut *self.inner.write() {
                    Inner::Init(init)=>init.bind_path(path)?,
                    _ =>{
                        log::error!("socket has listen or connected");
                        return Err(SystemError::EINVAL);
                    }
                };
                INODE_MAP.write_irqsave().insert(inodeid, inode);
                Ok(())
            }
            _ => return Err(SystemError::EINVAL)
        }
    }

    fn shutdown(&self, _stype: ShutdownTemp) -> Result<(), SystemError> {
        todo!();
    }

    fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        let mut inner = self.inner.write();
        let epoint = match & *inner {
            Inner::Init(init) => {
                init.endpoint().ok_or(SystemError::EINVAL)?.clone()
            }
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
            match self.try_accept().map(|(stream_socket, remote_endpoint)| {
                (stream_socket, Endpoint::from(remote_endpoint))
            }) {
                Ok((socket, endpoint)) => {
                    debug!("server accept!:{:?}", endpoint);
                    return Ok((socket, endpoint))
                },
                Err(_) => {
                    continue
                },
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

    fn wait_queue(&self) -> &WaitQueue {
        return &self.wait_queue;
    }

    fn poll(&self) -> usize {
        todo!()
    }

    fn close(&self) -> Result<(), SystemError> {
        Ok(())
    }

    fn get_peer_name(&self) -> Result<Endpoint, SystemError> {
        //获取对端地址
        let endpoint = match  &*self.inner.read() {
            Inner::Connected(connected) => connected.peer_endpoint().cloned(),
            _ =>return Err(SystemError::ENOTCONN)
        };
        
        if let Some(endpoint) = endpoint{
            return Ok(Endpoint::from(endpoint));
        }
        else {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
    }

    fn get_name(&self) -> Result<Endpoint, SystemError> {
        //获取本端地址
        let endpoint = match & *self.inner.read() {
            Inner::Init(init) => init.endpoint().cloned(),
            Inner::Connected(connected) => connected.endpoint().cloned(),
            Inner::Listener(listener) => listener.endpoint().cloned(),
        };

        if let Some(endpoint) = endpoint{
            return Ok(Endpoint::from(endpoint));
        }
        else {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
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
        debug!("stream recv!");
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
                    Ok(len) => {
                        debug!("stream recv finish!");                     
                        return Ok(len)
                    },
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
        debug!("stream recv from!");
        match & *self.inner.write() {
            Inner::Connected(connected) => {
                if flags.contains(MessageFlag::OOB) {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                if !flags.contains(MessageFlag::DONTWAIT) {
                    loop {
                        match connected.try_recv(buffer) {
                            Ok(usize) => return Ok((usize, connected.peer_endpoint().unwrap().clone())),
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
        debug!("stream socket send!");
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
                    Ok(len) => {
                        debug!("stream socket finish send!");
                        return Ok(len)
                    },
                    Err(_) => {
                        continue
                    },
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
        _buffer: &[u8],
        _flags: socket::MessageFlag,
        _address: Endpoint,
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
