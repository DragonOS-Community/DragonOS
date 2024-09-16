pub mod inner;
use core::sync::atomic::{AtomicBool, Ordering};
use alloc::sync::{Arc,Weak};

use inner::*;
use system_error::SystemError;

use crate::{libs::rwlock::RwLock, 
            net::socket::{common::{poll_unit::{EPollItems, WaitQueue,}, shutdown, Shutdown}, 
            endpoint::Endpoint, inode::Inode, Socket}};
use crate::net::{event_poll::EPollEventType,socket::MessageFlag};


type EP = EPollEventType;
#[derive(Debug)]
pub struct SeqpacketSocket{
    inner:RwLock<Inner>,
    shutdown: Shutdown,
    is_nonblocking: AtomicBool,
    epitems: EPollItems,
    wait_queue: WaitQueue,
    self_ref: Weak<Self>,
}

impl SeqpacketSocket {
    /// 默认的元数据缓冲区大小
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的缓冲区大小
    pub const DEFAULT_BUF_SIZE: usize = 64 * 1024;
    
    pub fn new(is_nonblocking: bool)-> Arc<Self>{
        Arc::new_cyclic(|me|Self{
            inner: RwLock::new(Inner::Init(Init::new())),
            shutdown:Shutdown::new(),
            is_nonblocking:AtomicBool::new(is_nonblocking),
            epitems:EPollItems::default(),
            wait_queue:WaitQueue::default(),
            self_ref: me.clone(),
        })
    }

    pub fn new_connected(connected: Connected, is_nonblocking: bool) ->Arc<Self>{
        Arc::new_cyclic(|me|Self{
            inner:RwLock::new(Inner::Connected(connected)),
            shutdown:Shutdown::new(),
            is_nonblocking:AtomicBool::new(is_nonblocking),
            epitems:EPollItems::default(),
            wait_queue:WaitQueue::default(),
            self_ref: me.clone(),
        })
    }

    pub fn new_pairs() ->Result<(Arc<Inode>,Arc<Inode>),SystemError> {
        let socket0=SeqpacketSocket::new(false);
        let socket1=SeqpacketSocket::new(false);
        let inode0=Inode::new(socket0.clone());
        let inode1=Inode::new(socket1.clone());

        let (conn_0, conn_1)=Connected::new_pair(Some(Endpoint::Inode(inode0.clone())), Some(Endpoint::Inode(inode1.clone())));
        *socket0.inner.write()=Inner::Connected(conn_0);
        *socket1.inner.write()=Inner::Connected(conn_1);

        return Ok((inode0, inode1))
    }

    fn try_accept(&self) -> Result<(Arc<Inode>, Endpoint),SystemError> {
        match &*self.inner.read() {
            Inner::Listen(listen) => listen.try_accept() as _,
            _ => {
                log::error!("the socket is not listening");
                return Err(SystemError::EINVAL)
                }
        }
    }

    fn is_nonblocking(&self) -> bool {
        self.is_nonblocking.load(Ordering::Relaxed)
    }

    fn set_nonblocking(&self, nonblocking: bool) {
        self.is_nonblocking.store(nonblocking, Ordering::Relaxed);
    }

}



impl Socket for SeqpacketSocket{
    fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        let peer_inode = match endpoint {
            Endpoint::Inode(inode)=> inode,
            _ => return Err(SystemError::EINVAL),
        };
        //let remote_socket=Arc::downcast::<SeqpacketSocket>(peer_inode.inner());
        let remote_socket=Arc::downcast::<SeqpacketSocket>(peer_inode.inner()).map_err(|_| SystemError::EINVAL)?;
        let client_epoint = match  &*self.inner.read() {
            Inner::Init(init) => init.endpoint().cloned(),
            Inner::Listen(_) => return Err(SystemError::EINVAL),
            Inner::Connected(_) => return Err(SystemError::EISCONN),
        };
        // ***
        match &*remote_socket.inner.read() {
            Inner::Listen(listener) => match listener.push_incoming(client_epoint){
                Ok(connected) => {
                     *self.inner.write() = Inner::Connected(connected);
                    return Ok(());
            },
            // ***错误处理
                Err(_) => todo!(),
            },
            Inner::Init(_) => {
                log::debug!("init einval");
                return Err(SystemError::EINVAL)},
            Inner::Connected(_) => return Err(SystemError::EISCONN),
        };

    }

    fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        match &mut *self.inner.write(){
            Inner::Init(init)=>init.bind(endpoint),
            _ =>{
                log::error!("cannot bind a listening or connected socket");
                return Err(SystemError::EINVAL)
            }
        }
    }

    fn shutdown(&self, how: shutdown::ShutdownTemp) -> Result<(), SystemError> {
        match &*self.inner.write(){
            Inner::Connected(connected)=>{
                connected.shutdown(how)
            }
            _ =>Err(SystemError::EINVAL)
        }
        
    }

    fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        let mut state =self.inner.write();

        let epoint = match &*state{
            Inner::Init(init) => init.endpoint().ok_or(SystemError::EINVAL)?.clone(),
            Inner::Listen(listener) => return listener.listen(backlog),
            Inner::Connected(_) =>{
                log::error!("the socket is connected");
                return Err(SystemError::EINVAL);
            },
        };

        let listener = Listener::new(epoint, backlog);
        *state = Inner::Listen(listener);

        Ok(())
    }

    fn accept(&self) -> Result<(Arc<Inode>, Endpoint), SystemError> {
        if !self.is_nonblocking() {
            self.try_accept().map(|(seqpacket_socket, remote_endpoint)|{
                (seqpacket_socket,Endpoint::from(remote_endpoint))
            })
        } else {
            // ***非阻塞状态
            todo!()
        }
    }

    fn set_option(
        &self,
        _level: crate::net::socket::OptionsLevel,
        _optname: usize,
        _optval: &[u8],
    ) -> Result<(), SystemError> {
        log::warn!("setsockopt is not implemented");
        Ok(())
    }

    
    fn wait_queue(&self) -> WaitQueue {
        return self.wait_queue.clone();
    }
    
    fn close(&self) -> Result<(), SystemError> {
        Ok(())
    }
    
    fn get_peer_name(&self) -> Result<Endpoint, SystemError> {
        // 获取对端地址
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
        let endpoint = match  &*self.inner.read() {
            Inner::Init(init) => init.endpoint().cloned(),
            Inner::Listen(listener) => Some(listener.endpoint().clone()),
            Inner::Connected(connected) => connected.endpoint().cloned(),
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
        level: crate::net::socket::OptionsLevel,
        name: usize,
        value: &mut [u8],
    ) -> Result<usize, SystemError> {
        log::warn!("getsockopt is not implemented");
        Ok(0)
    }
    
    fn read(&self, buffer: &mut [u8]) -> Result<usize, SystemError> {
        self.recv(buffer, crate::net::socket::MessageFlag::empty())
    }
    
    fn recv(&self, buffer: &mut [u8], flags: crate::net::socket::MessageFlag) -> Result<usize, SystemError> {
        match &*self.inner.write(){
            Inner::Connected(connected)=>{
                if flags.contains(MessageFlag::OOB){
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                if !flags.contains(MessageFlag::DONTWAIT){
                    loop{
                        match connected.try_read(buffer){
                            Ok(usize)=>return Ok(usize),
                            Err(_)=>continue,
                        }
                    }
                }
                else {
                    unimplemented!("unimplemented non_block")
                }
            },
            _=>{
                log::error!("the socket is not connected");
                return Err(SystemError::ENOTCONN)
            }
        }
        
    }
    
    
    fn recv_msg(&self, msg: &mut crate::net::syscall::MsgHdr, flags: crate::net::socket::MessageFlag) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }
    
    fn send(&self, buffer: &[u8], flags: crate::net::socket::MessageFlag) -> Result<usize, SystemError> {
            match &mut *self.inner.write() {
            Inner::Connected(connected)=>{
                if flags.contains(MessageFlag::OOB){
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }

                if !flags.contains(MessageFlag::DONTWAIT){
                    loop{
                        match connected.try_write(buffer){
                            Ok(usize)=>return Ok(usize),
                            Err(_)=>continue,
                        }
                    }
                }
                else {
                    unimplemented!("unimplemented non_block")
                }
            },
            _ =>{
                log::error!("the socket is not connected");
                return Err(SystemError::ENOTCONN)
            }
        }
    }
    
    fn send_msg(&self, msg: &crate::net::syscall::MsgHdr, flags: crate::net::socket::MessageFlag) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }
    
    
    fn write(&self, buffer: &[u8]) -> Result<usize, SystemError> {
        self.send(buffer, crate::net::socket::MessageFlag::empty())
    }

    fn recv_from(
            &self, 
            buffer: &mut [u8],
            len: usize,
            flags: MessageFlag,
            _address: Option<Endpoint>,
        ) -> Result<(usize, Endpoint), SystemError> {
        match &*self.inner.write(){
            Inner::Connected(connected)=>{
                if flags.contains(MessageFlag::OOB){
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                if !flags.contains(MessageFlag::DONTWAIT){
                    loop{
                        match connected.recv_slice(buffer){
                            Ok(usize)=>return Ok((usize,connected.endpoint().unwrap().clone())),
                            Err(_)=>continue,
                        }
                    }
                }
                else {
                    unimplemented!("unimplemented non_block")
                }
            },
            _=>{
                log::error!("the socket is not connected");
                return Err(SystemError::ENOTCONN)
            }
        }
        //Err(SystemError::ENOSYS) 
    }
    
    
    // fn update_io_events(&self) -> Result<crate::net::socket::EPollEventType, SystemError> {
    //     // 参考linux的unix_poll https://code.dragonos.org.cn/xref/linux-6.1.9/net/unix/af_unix.c#3152
    //     todo!()
    //     // let mut mask = EP::empty();
    //     // let shutdown = self.shutdown.get();
        
    //     // // todo:socket_poll_wait?注册socket
    //     // if shutdown.is_both_shutdown(){
    //     //     mask |= EP::EPOLLHUP;
    //     // }

    //     // if shutdown.is_recv_shutdown(){
    //     //     mask |= EP::EPOLLRDHUP | EP::EPOLLIN | EP::EPOLLRDNORM;
    //     // }
    //     // match &*self.state.read(){
    //     //     State::Connected(connected) => {
    //     //         if connected.can_recv(){
    //     //             mask |= EP::EPOLLIN | EP::EPOLLRDNORM;
    //     //         }
    //     //         // if (sk_is_readable(sk))
    //     //         // mask |= EPOLLIN | EPOLLRDNORM;

    //     //         // TODO:处理紧急情况 EPOLLPRI
    //     //         // TODO:处理连接是否关闭 EPOLLHUP
    //     //         if !shutdown.is_send_shutdown() {
    //     //             if connected.can_send() {
    //     //                 mask |= EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND;
    //     //             } else {
    //     //                 todo!("TcpSocket::poll: buffer space not enough");
    //     //             }
    //     //         } else {
    //     //             mask |= EP::EPOLLOUT | EP::EPOLLWRNORM;
    //     //         }
    //     //     },
    //     //     _ =>return Err(SystemError::EINVAL),
    //     // }
    //     // return Ok(mask)
    // }
    
    fn send_buffer_size(&self) -> usize {
        todo!()
    }
    
    fn recv_buffer_size(&self) -> usize {
        todo!()
    }
    
    fn poll(&self) -> usize {
        todo!()
    }
    
}