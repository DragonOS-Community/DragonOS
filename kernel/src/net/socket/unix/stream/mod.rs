use alloc::{sync::{Arc,Weak}, vec::Vec};
use inner::{Connected, Init, Inner, Listener};
use intertrait::CastFromSync;
use system_error::SystemError;

use crate::{
    filesystem::vfs::IndexNode, libs::{rwlock::RwLock, spinlock::SpinLock}, net::{
        socket::{
            common::{
                poll_unit::{EPollItems, WaitQueue},
                Shutdown,
            },
            Socket,
        },
        Endpoint,
    }
};

pub mod inner;

#[derive(Debug)]
pub struct StreamSocket {
    read_buffer: Arc<SpinLock<Vec<u8>>>,
    write_buffer: Arc<SpinLock<Vec<u8>>>,
    inner: RwLock<Option<Inner>>,
    shutdown: Shutdown,
    self_ref: Weak<Self>,
}

impl StreamSocket {
    /// 默认的元数据缓冲区大小
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的缓冲区大小
    pub const DEFAULT_BUF_SIZE: usize = 64 * 1024;

    pub fn new() -> Arc<Self> {
        Arc::new(Self{
            read_buffer: Arc::new(SpinLock::new(Vec::new())),
            write_buffer: Arc::new(SpinLock::new(Vec::new())),
            inner: RwLock::new(Some(Inner::Init(Init::new()))),
            shutdown: Shutdown::new(),
            self_ref: Weak::new(),
        })
    }

    pub fn new_connected(connected: Connected) -> Arc<Self> {
        Arc::new(Self{
            read_buffer: Arc::new(SpinLock::new(Vec::new())),
            write_buffer: Arc::new(SpinLock::new(Vec::new())),
            inner: RwLock::new(Some(Inner::Connected(connected))),
            shutdown: Shutdown::new(),
            self_ref: Weak::new(),
        })
    }

    pub fn do_bind(&self, local_endpoint: Endpoint) -> Result<(), SystemError> {
        let mut guard = self.inner.write();
        match guard.take().expect("Unix Stream Socket is None") {
            Inner::Init(mut inner) => {
                inner.bind(local_endpoint)?;
                guard.replace(Inner::Init(inner));
                Ok(())
            },
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
            },
            _ => return Err(SystemError::EINVAL),
        }
    }

    pub fn do_accept(&self) -> Result<(Arc<StreamSocket>, Endpoint), SystemError> {
        let mut inner = self.inner.write();
        match inner.take().expect("Unix Stream Socket is None") {
            Inner::Listener(listener) => {
                let server_conn = listener.pop_incoming();
                let peer_addr = server_conn.clone().take().expect("Unix Stream Socket is none")
                .peer_addr().unwrap();
                
                return Ok((StreamSocket::new_connected(server_conn.unwrap()), peer_addr));
                
            },
            _ => {
                return Err(SystemError::EINVAL);
            }
        }
    }
    
}


impl Socket for StreamSocket {
    
    fn connect(&self, _endpoint: Endpoint) -> Result<(), SystemError> {
        //使用endpoint获取服务端socket
        let remote_socket = match _endpoint {
            Endpoint::Inode(socket) => socket,
            _ => return Err(SystemError::EINVAL),
        };

        //客户端建立connected连接
        let mut client_socket = self.inner.write();
        match client_socket.take().expect("Unix Stream Socket is None") {
            Inner::Init(inner) => {
                let client_conn = Connected::new(inner.addr().clone(), remote_socket.endpoint());
                client_socket.replace(Inner::Connected(client_conn));
            },
            _ => {
                return Err(SystemError::EINVAL);
            }
        }

        let remote_stream_socket: Arc<StreamSocket> = Arc::clone(&remote_socket).arc_any().downcast().map_err(|_| SystemError::EINVAL)?;
        
        //服务端建立连接
        return self.do_connect(remote_stream_socket);
    }
    
    fn bind(&self, _endpoint: Endpoint) -> Result<(), SystemError> {
        return self.do_bind(_endpoint);
    }
    
    fn shutdown(&self, _type: Shutdown) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }
    
    fn listen(&self, _backlog: usize) -> Result<(), SystemError> {
        return self.do_listen(_backlog);
    }
    
    fn accept(&self) -> Result<(Arc<dyn crate::filesystem::vfs::IndexNode>, Endpoint), SystemError> {
        self.do_accept().map(|(stream, remote_endpoint)|
            (stream as Arc<dyn IndexNode>, Endpoint::from(remote_endpoint))
        )
    }
    
    fn set_option(
        &self,
        _level: crate::net::socket::SocketOptionsLevel,
        _optname: usize,
        _optval: &[u8],
    ) -> Result<(), SystemError> {
        log::warn!("setsockopt is not implemented");
        Ok(())
    }
    
    fn endpoint(&self) -> Option<Endpoint> {
        None
    }
    
    fn peer_endpoint(&self) -> Option<Endpoint> {
        None
    }
    
    fn write_buffer(&self, _buf: &[u8]) -> Result<usize, SystemError> {
        core::todo!()
    }
    
    fn epoll_items(&self) -> EPollItems {
        todo!()
    }
    
    fn wait_queue(&self) -> WaitQueue {
        todo!()
    }
    
    fn update_io_events(&self) -> Result<crate::net::event_poll::EPollEventType, SystemError> {
        todo!()
    }
    
}
