pub mod inner;
use core::sync::atomic::{AtomicBool, Ordering};
use alloc::{sync::{Arc,Weak},string::String};

use inner::*;
use system_error::SystemError;
use crate::sched::SchedMode;
use crate::{libs::rwlock::RwLock, net::socket::*};

use super::INODE_MAP;


type EP = EPollEventType;
#[derive(Debug)]
pub struct SeqpacketSocket{
    inner:RwLock<Inner>,
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
    
    pub fn new(is_nonblocking: bool)-> Arc<Self>{
        Arc::new_cyclic(|me|Self{
            inner: RwLock::new(Inner::Init(Init::new())),
            shutdown:Shutdown::new(),
            is_nonblocking:AtomicBool::new(is_nonblocking),
            wait_queue:WaitQueue::default(),
            self_ref: me.clone(),
        })
    }

    pub fn new_inode(is_nonblocking: bool)-> Result<Arc<Inode>, SystemError>{
        let socket = SeqpacketSocket::new(is_nonblocking);
        let inode = Inode::new(socket.clone());
        // 建立时绑定自身为后续能正常获取本端地址
        let _ = match &mut*socket.inner.write() {
            Inner::Init(init)=>init.bind(Endpoint::Inode((inode.clone(),String::from("")))),
            _=>return Err(SystemError::EINVAL),
        };
        return Ok(inode);
    }

    pub fn new_connected(connected: Connected, is_nonblocking: bool) ->Arc<Self>{
        Arc::new_cyclic(|me|Self{
            inner:RwLock::new(Inner::Connected(connected)),
            shutdown:Shutdown::new(),
            is_nonblocking:AtomicBool::new(is_nonblocking),
            wait_queue:WaitQueue::default(),
            self_ref: me.clone(),
        })
    }

    pub fn new_pairs() ->Result<(Arc<Inode>,Arc<Inode>),SystemError> {
        let socket0=SeqpacketSocket::new(false);
        let socket1=SeqpacketSocket::new(false);
        let inode0=Inode::new(socket0.clone());
        let inode1=Inode::new(socket1.clone());

        let (conn_0, conn_1)=Connected::new_pair(Some(Endpoint::Inode((inode0.clone(),String::from("")))), Some(Endpoint::Inode((inode1.clone(),String::from("")))));
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

    fn is_acceptable(&self)-> bool{
        match &*self.inner.read() {
            Inner::Listen(listen) => listen.is_acceptable(),
            _ => {
                panic!("the socket is not listening");
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
            Endpoint::Inode((inode,_))=> inode,
            Endpoint::Unixpath((inode_id,_))=>{
                let inode_guard = INODE_MAP.read_irqsave();
                let inode = inode_guard.get(&inode_id).unwrap();
                match inode {
                Endpoint::Inode((inode,_))=> inode.clone(),
                _ => {return Err(SystemError::EINVAL)},
                }
            }
            _ => return Err(SystemError::EINVAL),
        };
        // 远端为服务端
        let remote_socket=Arc::downcast::<SeqpacketSocket>(peer_inode.inner()).map_err(|_| SystemError::EINVAL)?;
        
        let client_epoint = match  &mut *self.inner.write() {
            Inner::Init(init) => {
                match init.endpoint().cloned(){
                    Some(end)=>{
                        log::debug!("bind when connect");
                        Some(end)
                    },
                    None=>{
                        log::debug!("not bind when connect");
                        let inode= Inode::new(self.self_ref.upgrade().unwrap().clone());
                        let epoint = Endpoint::Inode((inode.clone(),String::from("")));
                        let _ = init.bind(epoint.clone());
                        Some(epoint)
                    }
                }
            },
            Inner::Listen(_) => return Err(SystemError::EINVAL),
            Inner::Connected(_) => return Err(SystemError::EISCONN),
        };
        // ***阻塞与非阻塞处理还未实现
        // 客户端与服务端建立连接将服务端inode推入到自身的listen_incom队列中，
        // accept时从中获取推出对应的socket
        match &*remote_socket.inner.read() {
            Inner::Listen(listener) => match listener.push_incoming(client_epoint){
                Ok(connected) => {
                     *self.inner.write() = Inner::Connected(connected);

                     remote_socket.wait_queue.wakeup(None);
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
        // 将自身socket的inode与用户端提供路径的文件indoe_id进行绑定
        match endpoint{
            Endpoint::Unixpath((inodeid,path))=>{
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
            _=>return Err(SystemError::EINVAL)
        }
           
        // match &mut *self.inner.write(){
        //     Inner::Init(init)=>init.bind(Endpoint::Inode(inode.clone())),
        //     _ =>{
        //         log::error!("cannot bind a listening or connected socket");
        //         return Err(SystemError::EINVAL)
        //     }
        // }
    }

    fn shutdown(&self, how: ShutdownTemp) -> Result<(), SystemError> {
        match &*self.inner.write(){
            Inner::Connected(connected)=>{
                connected.shutdown(how)
            }
            _ =>Err(SystemError::EINVAL)
        }
        
    }

    fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        let mut state =self.inner.write();
        log::debug!("listen into socket");
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
            loop {
                wq_wait_event_interruptible!(self.wait_queue, self.is_acceptable(), {})?;
                match self.try_accept().map(|(seqpacket_socket, remote_endpoint)|{
                    (seqpacket_socket,Endpoint::from(remote_endpoint))
                }){
                    Ok((socket,epoint))=>return Ok((socket,epoint)),
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
        _level: crate::net::socket::OptionsLevel,
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
        // 获取本端地址
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
                            Ok(usize)=>{
                                log::debug!("recv successfully");
                                return Ok(usize)
                            },
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
                            Ok(usize)=>{
                                log::debug!("send successfully");
                                return Ok(usize)
                            },
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
            flags: MessageFlag,
            _address: Option<Endpoint>,
        ) -> Result<(usize, Endpoint), SystemError> {
        log::debug!("recvfrom flags {:?}",flags);

        // wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
        // connect锁和flag判断顺序不正确，应该先判断在
        match &*self.inner.write(){
            Inner::Connected(connected)=>{
                if flags.contains(MessageFlag::OOB){
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                if !flags.contains(MessageFlag::DONTWAIT){
                    loop{
                        match connected.recv_slice(buffer){
                            Ok(usize)=>{
                                log::debug!("recv from successfully");
                                return Ok((usize,connected.peer_endpoint().unwrap().clone()))
                            },
                            Err(_) => continue,
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
        log::warn!("using default buffer size");
        SeqpacketSocket::DEFAULT_BUF_SIZE
    }
    
    fn recv_buffer_size(&self) -> usize {
        log::warn!("using default buffer size");
        SeqpacketSocket::DEFAULT_BUF_SIZE
    }
    
    fn poll(&self) -> usize {
        EPollEventType::empty().bits() as usize
    }
    
}