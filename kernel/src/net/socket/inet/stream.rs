use alloc::{boxed::Box, sync::Arc, vec::Vec};
use log::{debug, error, warn};
use smoltcp::{
    socket::{raw, tcp, udp},
    wire,
};
use system_error::SystemError;

use crate::{
    driver::net::Iface,
    libs::rwlock::RwLock,
    net::{
        event_poll::EPollEventType, net_core::poll_ifaces, socket::tcp_def::TcpOptions, syscall::PosixSocketOption, Endpoint, Protocol, ShutdownType, NET_DEVICES, SocketOptionsLevel
    },
};

use crate::net::socket::{
    handle::GlobalSocketHandle, PosixSocketHandleItem, Socket, SocketHandleItem, SocketMetadata,
    SocketOptions, SocketPollMethod, InetSocketType, HANDLE_MAP, PORT_MANAGER, SOCKET_SET, ip_def::IpOptions,
};


/// @brief 表示 tcp socket
///
/// https://man7.org/linux/man-pages/man7/tcp.7.html
#[derive(Debug, Clone)]
pub struct TcpSocket {
    handles: Vec<GlobalSocketHandle>,
    local_endpoint: Option<wire::IpEndpoint>, // save local endpoint for bind()
    is_listening: bool,
    metadata: SocketMetadata,
    posix_item: Arc<PosixSocketHandleItem>,
}

impl TcpSocket {
    /// 元数据的缓冲区的大小
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的接收缓冲区的大小 receive
    pub const DEFAULT_RX_BUF_SIZE: usize = 512 * 1024;
    /// 默认的发送缓冲区的大小 transmiss
    pub const DEFAULT_TX_BUF_SIZE: usize = 512 * 1024;

    /// TcpSocket的特殊事件，用于在事件等待队列上sleep
    pub const CAN_CONNECT: u64 = 1u64 << 63;
    pub const CAN_ACCPET: u64 = 1u64 << 62;

    /// @brief 创建一个tcp的socket
    ///
    /// @param options socket的选项
    ///
    /// @return 返回创建的tcp的socket
    pub fn new(options: SocketOptions) -> Self {
        // 创建handles数组并把socket添加到socket集合中，并得到socket的句柄
        let handles: Vec<GlobalSocketHandle> = vec![GlobalSocketHandle::new_smoltcp_handle(
            SOCKET_SET.lock_irqsave().add(Self::create_new_socket()),
        )];

        let metadata = SocketMetadata::new(
            InetSocketType::Tcp,
            Self::DEFAULT_RX_BUF_SIZE,
            Self::DEFAULT_TX_BUF_SIZE,
            Self::DEFAULT_METADATA_BUF_SIZE,
            options,
        );
        let posix_item = Arc::new(PosixSocketHandleItem::new(None));
        // debug!("when there's a new tcp socket,its'len: {}",handles.len());

        return Self {
            handles,
            local_endpoint: None,
            is_listening: false,
            metadata,
            posix_item,
        };
    }

    fn do_listen(
        &mut self,
        socket: &mut tcp::Socket,
        local_endpoint: wire::IpEndpoint,
    ) -> Result<(), SystemError> {
        let listen_result = if local_endpoint.addr.is_unspecified() {
            socket.listen(local_endpoint.port)
        } else {
            socket.listen(local_endpoint)
        };
        return match listen_result {
            Ok(()) => {
                // debug!(
                //     "Tcp Socket Listen on {local_endpoint}, open?:{}",
                //     socket.is_open()
                // );
                self.is_listening = true;

                Ok(())
            }
            Err(_) => Err(SystemError::EINVAL),
        };
    }

    /// # create_new_socket - 创建新的TCP套接字
    ///
    /// 该函数用于创建一个新的TCP套接字，并返回该套接字的引用。
    fn create_new_socket() -> tcp::Socket<'static> {
        // 初始化tcp的buffer
        let rx_buffer = tcp::SocketBuffer::new(vec![0; Self::DEFAULT_RX_BUF_SIZE]);
        let tx_buffer = tcp::SocketBuffer::new(vec![0; Self::DEFAULT_TX_BUF_SIZE]);
        tcp::Socket::new(rx_buffer, tx_buffer)
    }

    /// listening状态的posix socket是需要特殊处理的
    fn tcp_poll_listening(&self) -> EPollEventType {
        let socketset_guard = SOCKET_SET.lock_irqsave();

        let can_accept = self.handles.iter().any(|h| {
            if let Some(sh) = h.smoltcp_handle() {
                let socket = socketset_guard.get::<tcp::Socket>(sh);
                socket.is_active()
            } else {
                false
            }
        });

        if can_accept {
            return EPollEventType::EPOLL_LISTEN_CAN_ACCEPT;
        } else {
            return EPollEventType::empty();
        }
    }

    fn sk_setsockopt(
        &self,
        _socket: &mut tcp::Socket,
        _level: SocketOptionsLevel,
        optname: PosixSocketOption,
        _optval: &[u8],
    ) -> Result<(), SystemError> {
        use PosixSocketOption::*;
        use SystemError::*;

        debug!("[SYS] [TCP] [setsockopt: {:?}]", optname);

        if optname == SO_BINDTODEVICE {
            todo!("SO_BINDTODEVICE");
        }

        match optname {
            SO_REUSEADDR => {
                return Ok(());
            }
            SO_TYPE => {}
            SO_PROTOCOL => {}
            SO_DOMAIN => {}
            SO_ERROR => {
                return Err(ENOPROTOOPT);
            }
            
            SO_TIMESTAMP_OLD => {}
            SO_TIMESTAMP_NEW => {}
            SO_TIMESTAMPNS_OLD => {}
            SO_TIMESTAMPING_OLD => {}
            SO_RCVTIMEO_OLD => {}
            SO_SNDTIMEO_OLD => {}

            // if define CONFIG_NET_RX_BUSY_POLL
            SO_BUSY_POLL | SO_PREFER_BUSY_POLL | SO_BUSY_POLL_BUDGET => {
                debug!("[SYS] [TCP] [setsockopt: {:?}] not supported", optname);
                return Err(ENOPROTOOPT);
            }
            // end if

            opt => {
                debug!("[SYS] [TCP] [setsockopt: {:?}] not supported", opt);
                return Err(ENOPROTOOPT);
            }
        }
        return Ok(());
    }

    fn do_tcp_setsockopt(
        &self,
        socket: &mut tcp::Socket,
        _level: SocketOptionsLevel,
        optname: usize,
        optval: &[u8],
    ) -> Result<(), SystemError> {

        let boolval = optval[0] != 0;

        match TcpOptions::from_bits_truncate(optname as u32) {
            TcpOptions::TCP_CONGESTION => {
                todo!("TCP_CONGESTION");
            }
            TcpOptions::TCP_QUICKACK => {
                if boolval {
                    socket.set_ack_delay(None);
                } else {
                    socket.set_ack_delay(Some(smoltcp::time::Duration::from_millis(10)));
                }
            }
            TcpOptions::TCP_NODELAY => {
                socket.set_nagle_enabled(boolval);
            }
            TcpOptions::TCP_USER_TIMEOUT => {
                let duration = u32::from_ne_bytes(optval.try_into().map_err(|_| SystemError::EINVAL)?) as u64;
                socket.set_timeout(Some(smoltcp::time::Duration::from_millis(duration)));
            }
            TcpOptions::TCP_KEEPINTVL => {
                let duration = u32::from_ne_bytes(optval.try_into().map_err(|_| SystemError::EINVAL)?) as u64;
                socket.set_keep_alive(Some(smoltcp::time::Duration::from_millis(duration)));
            }
            // TcpOptions::TCP_NL
            _ => {
                return Err(SystemError::ENOPROTOOPT);
            }
        }
        return Ok(());
    }
    
    fn do_ip_setsockopt(
        &self,
        _level: SocketOptionsLevel,
        optname: usize,
        _optval: &[u8],
    ) -> Result<(), SystemError> {
        debug!("ip_setsockopt: optname={}", optname);
        match IpOptions::from_bits_truncate(optname as u32) {
            IpOptions::IP_LOCAL_PORT_RANGE => {}
            _ => {}
        }
        return Ok(());
    }
}

impl Socket for TcpSocket {

    fn close(&mut self) {
        for handle in self.handles.iter() {
            {
                let mut socket_set_guard = SOCKET_SET.lock_irqsave();
                let smoltcp_handle = handle.smoltcp_handle().unwrap();
                socket_set_guard
                    .get_mut::<smoltcp::socket::tcp::Socket>(smoltcp_handle)
                    .close();
                drop(socket_set_guard);
            }
            poll_ifaces();
            SOCKET_SET
                .lock_irqsave()
                .remove(handle.smoltcp_handle().unwrap());
            // debug!("[Socket] [TCP] Close: {:?}", handle);
        }
    }

    fn read(&self, buf: &mut [u8]) -> (Result<usize, SystemError>, Endpoint) {
        if HANDLE_MAP
            .read_irqsave()
            .get(&self.socket_handle())
            .unwrap()
            .shutdown_type()
            .contains(ShutdownType::RCV_SHUTDOWN)
        {
            return (Err(SystemError::ENOTCONN), Endpoint::Ip(None));
        }

        let mut socket_set_guard = SOCKET_SET.lock_irqsave();

        let socket = socket_set_guard
            .get_mut::<tcp::Socket>(self.handles.first().unwrap().smoltcp_handle().unwrap());

        // 如果socket已经关闭，返回错误
        if !socket.is_active() {
            // debug!("Tcp Socket Read Error, socket is closed");
            return (Err(SystemError::ENOTCONN), Endpoint::Ip(None));
        }

        if socket.may_recv() {
            match socket.recv_slice(buf) {
                Ok(0) => {
                    return (Err(SystemError::EAGAIN_OR_EWOULDBLOCK), Endpoint::Ip(None));
                }
                Ok(size) => { // size: usize > 0
                    let endpoint = if let Some(p) = socket.remote_endpoint() {
                        p
                    } else {
                        return (Err(SystemError::ENOTCONN), Endpoint::Ip(None));
                    };

                    drop(socket_set_guard);
                    return (Ok(size), Endpoint::Ip(Some(endpoint)));
                }
                Err(tcp::RecvError::InvalidState) => {
                    warn!("Tcp Socket Read Error, InvalidState");
                    return (Err(SystemError::ENOTCONN), Endpoint::Ip(None));
                }
                Err(tcp::RecvError::Finished) => {
                    // 对端写端已关闭，我们应该关闭读端
                    HANDLE_MAP
                        .write_irqsave()
                        .get_mut(&self.socket_handle())
                        .unwrap()
                        .shutdown_type_writer()
                        .insert(ShutdownType::RCV_SHUTDOWN);
                    return (Err(SystemError::ENOTCONN), Endpoint::Ip(None));
                }
            }
        } else {
            return (Err(SystemError::ENOTCONN), Endpoint::Ip(None));
        }

    }

    fn write(&self, buf: &[u8], _to: Option<Endpoint>) -> Result<usize, SystemError> {
        if HANDLE_MAP
            .read_irqsave()
            .get(&self.socket_handle())
            .unwrap()
            .shutdown_type()
            .contains(ShutdownType::RCV_SHUTDOWN)
        {
            return Err(SystemError::ENOTCONN);
        }
        // debug!("tcp socket:write, socket'len={}",self.handle.len());

        let mut socket_set_guard = SOCKET_SET.lock_irqsave();

        let socket = socket_set_guard
            .get_mut::<tcp::Socket>(self.handles.first().unwrap().smoltcp_handle().unwrap());

        if socket.is_open() {
            if socket.can_send() {
                match socket.send_slice(buf) {
                    Ok(size) => {
                        drop(socket_set_guard);
                        poll_ifaces();
                        return Ok(size);
                    }
                    Err(e) => {
                        error!("Tcp Socket Write Error {e:?}");
                        return Err(SystemError::ENOBUFS);
                    }
                }
            } else {
                return Err(SystemError::ENOBUFS);
            }
        }

        return Err(SystemError::ENOTCONN);
    }

    fn poll(&self) -> EPollEventType {
        // 处理listen的快速路径
        if self.is_listening {
            return self.tcp_poll_listening();
        }
        // 由于上面处理了listening状态，所以这里只处理非listening状态，这种情况下只有一个handle

        assert!(self.handles.len() == 1);

        let mut socket_set_guard = SOCKET_SET.lock_irqsave();
        // debug!("tcp socket:poll, socket'len={}",self.handle.len());

        let socket = socket_set_guard
            .get_mut::<tcp::Socket>(self.handles.first().unwrap().smoltcp_handle().unwrap());
        let handle_map_guard = HANDLE_MAP.read_irqsave();
        let handle_item = handle_map_guard.get(&self.socket_handle()).unwrap();
        let shutdown_type = handle_item.shutdown_type();
        let is_posix_listen = handle_item.is_posix_listen;
        drop(handle_map_guard);

        return SocketPollMethod::tcp_poll(socket, shutdown_type, is_posix_listen);
    }

    fn connect(&mut self, endpoint: Endpoint) -> Result<(), SystemError> {
        let mut sockets = SOCKET_SET.lock_irqsave();
        // debug!("tcp socket:connect, socket'len={}", self.handles.len());

        let socket =
            sockets.get_mut::<tcp::Socket>(self.handles.first().unwrap().smoltcp_handle().unwrap());

        if let Endpoint::Ip(Some(ip)) = endpoint {
            let temp_port = PORT_MANAGER.get_ephemeral_port(self.metadata.socket_type)?;
            // 检测端口是否被占用
            PORT_MANAGER.bind_port(self.metadata.socket_type, temp_port)?;

            // debug!("temp_port: {}", temp_port);
            let iface: Arc<dyn Iface> = NET_DEVICES.write_irqsave().get(&0).unwrap().clone();
            let mut inner_iface = iface.inner_iface().lock();
            // debug!("to connect: {ip:?}");

            match socket.connect(inner_iface.context(), ip, temp_port) {
                Ok(()) => {
                    // avoid deadlock
                    drop(inner_iface);
                    drop(iface);
                    drop(sockets);
                    loop {
                        poll_ifaces();
                        let mut sockets = SOCKET_SET.lock_irqsave();
                        let socket = sockets.get_mut::<tcp::Socket>(
                            self.handles.first().unwrap().smoltcp_handle().unwrap(),
                        );

                        match socket.state() {
                            tcp::State::Established => {
                                return Ok(());
                            }
                            tcp::State::SynSent => {
                                drop(sockets);
                                self.posix_item.sleep(Self::CAN_CONNECT);
                            }
                            _ => {
                                return Err(SystemError::ECONNREFUSED);
                            }
                        }
                    }
                }
                Err(e) => {
                    // error!("Tcp Socket Connect Error {e:?}");
                    match e {
                        tcp::ConnectError::InvalidState => return Err(SystemError::EISCONN),
                        tcp::ConnectError::Unaddressable => return Err(SystemError::EADDRNOTAVAIL),
                    }
                }
            }
        } else {
            return Err(SystemError::EINVAL);
        }
    }

    /// @brief tcp socket 监听 local_endpoint 端口
    ///
    /// @param backlog 未处理的连接队列的最大长度
    fn listen(&mut self, backlog: usize) -> Result<(), SystemError> {
        if self.is_listening {
            return Ok(());
        }

        // debug!(
        //     "tcp socket:listen, socket'len={}, backlog = {backlog}",
        //     self.handles.len()
        // );

        let local_endpoint = self.local_endpoint.ok_or(SystemError::EINVAL)?;
        let mut sockets = SOCKET_SET.lock_irqsave();
        // 获取handle的数量
        let handlen = self.handles.len();
        let backlog = handlen.max(backlog);

        // 添加剩余需要构建的socket
        // debug!("tcp socket:before listen, socket'len={}", self.handle_list.len());
        let mut handle_guard = HANDLE_MAP.write_irqsave();
        let socket_handle_item_0 = handle_guard.get_mut(&self.socket_handle()).unwrap();
        socket_handle_item_0.is_posix_listen = true;

        self.handles.extend((handlen..backlog).map(|_| {
            let socket = Self::create_new_socket();
            let handle = GlobalSocketHandle::new_smoltcp_handle(sockets.add(socket));
            let mut handle_item = SocketHandleItem::new(Arc::downgrade(&self.posix_item));
            handle_item.is_posix_listen = true;
            handle_guard.insert(handle, handle_item);
            handle
        }));

        // debug!("tcp socket:listen, socket'len={}", self.handles.len());
        // debug!("tcp socket:listen, backlog={backlog}");

        // 监听所有的socket
        for i in 0..backlog {
            let handle = self.handles.get(i).unwrap();

            let socket = sockets.get_mut::<tcp::Socket>(handle.smoltcp_handle().unwrap());

            if !socket.is_listening() {
                // debug!("Tcp Socket is already listening on {local_endpoint}");
                self.do_listen(socket, local_endpoint)?;
            }
            // debug!("Tcp Socket  before listen, open={}", socket.is_open());
        }

        return Ok(());
    }

    fn bind(&mut self, endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(Some(mut ip)) = endpoint {
            if ip.port == 0 {
                ip.port = PORT_MANAGER.get_ephemeral_port(self.metadata.socket_type)?;
            }

            // 检测端口是否已被占用
            PORT_MANAGER.bind_port(self.metadata.socket_type, ip.port)?;
            // debug!("tcp socket:bind, socket'len={}",self.handle.len());

            self.local_endpoint = Some(ip);
            self.is_listening = false;

            return Ok(());
        }
        return Err(SystemError::EINVAL);
    }

    fn shutdown(&mut self, shutdown_type: crate::net::ShutdownType) -> Result<(), SystemError> {
        // TODO：目前只是在表层判断，对端不知晓，后续需使用tcp实现
        HANDLE_MAP
            .write_irqsave()
            .get_mut(&self.socket_handle())
            .unwrap()
            .shutdown_type = RwLock::new(shutdown_type);
        return Ok(());
    }

    fn accept(&mut self) -> Result<(Box<dyn Socket>, Endpoint), SystemError> {
        if !self.is_listening {
            return Err(SystemError::EINVAL);
        }
        let endpoint = self.local_endpoint.ok_or(SystemError::EINVAL)?;
        loop {
            // debug!("tcp accept: poll_ifaces()");
            poll_ifaces();
            // debug!("tcp socket:accept, socket'len={}", self.handle_list.len());

            let mut sockset = SOCKET_SET.lock_irqsave();
            // Get the corresponding activated handler
            let global_handle_index = self.handles.iter().position(|handle| {
                let con_smol_sock = sockset.get::<tcp::Socket>(handle.smoltcp_handle().unwrap());
                con_smol_sock.is_active()
            });

            if let Some(handle_index) = global_handle_index {
                let con_smol_sock = sockset
                    .get::<tcp::Socket>(self.handles[handle_index].smoltcp_handle().unwrap());

                // debug!("[Socket] [TCP] Accept: {:?}", handle);
                // handle is connected socket's handle
                let remote_ep = con_smol_sock
                    .remote_endpoint()
                    .ok_or(SystemError::ENOTCONN)?;

                let tcp_socket = Self::create_new_socket();

                let new_handle = GlobalSocketHandle::new_smoltcp_handle(sockset.add(tcp_socket));

                // let handle in TcpSock be the new empty handle, and return the old connected handle
                let old_handle = core::mem::replace(&mut self.handles[handle_index], new_handle);

                let metadata = SocketMetadata::new(
                    InetSocketType::Tcp,
                    Self::DEFAULT_TX_BUF_SIZE,
                    Self::DEFAULT_RX_BUF_SIZE,
                    Self::DEFAULT_METADATA_BUF_SIZE,
                    self.metadata.options,
                );

                let sock_ret = Box::new(TcpSocket {
                    handles: vec![old_handle],
                    local_endpoint: self.local_endpoint,
                    is_listening: false,
                    metadata,
                    posix_item: Arc::new(PosixSocketHandleItem::new(None)),
                });

                {
                    let mut handle_guard = HANDLE_MAP.write_irqsave();
                    // 先删除原来的
                    let item = handle_guard.remove(&old_handle).unwrap();
                    item.reset_shutdown_type();
                    assert!(item.is_posix_listen);

                    // 按照smoltcp行为，将新的handle绑定到原来的item
                    let new_item = SocketHandleItem::new(Arc::downgrade(&sock_ret.posix_item));
                    handle_guard.insert(old_handle, new_item);
                    // 插入新的item
                    handle_guard.insert(new_handle, item);

                    let socket = sockset.get_mut::<tcp::Socket>(
                        self.handles[handle_index].smoltcp_handle().unwrap(),
                    );

                    if !socket.is_listening() {
                        self.do_listen(socket, endpoint)?;
                    }

                    drop(handle_guard);
                }

                return Ok((sock_ret, Endpoint::Ip(Some(remote_ep))));
            }

            drop(sockset);

            // debug!("[TCP] [Accept] sleeping socket with handle: {:?}", self.handles.first().unwrap().smoltcp_handle().unwrap());
            self.posix_item.sleep(Self::CAN_ACCPET);
            // debug!("tcp socket:after sleep, handle_guard'len={}",HANDLE_MAP.write_irqsave().len());
        }
    }

    fn endpoint(&self) -> Option<Endpoint> {
        let mut result: Option<Endpoint> = self.local_endpoint.map(|x| Endpoint::Ip(Some(x)));

        if result.is_none() {
            let sockets = SOCKET_SET.lock_irqsave();
            // debug!("tcp socket:endpoint, socket'len={}",self.handle.len());

            let socket =
                sockets.get::<tcp::Socket>(self.handles.first().unwrap().smoltcp_handle().unwrap());
            if let Some(ep) = socket.local_endpoint() {
                result = Some(Endpoint::Ip(Some(ep)));
            }
        }
        return result;
    }

    fn peer_endpoint(&self) -> Option<Endpoint> {
        let sockets = SOCKET_SET.lock_irqsave();
        // debug!("tcp socket:peer_endpoint, socket'len={}",self.handle.len());

        let socket =
            sockets.get::<tcp::Socket>(self.handles.first().unwrap().smoltcp_handle().unwrap());
        return socket.remote_endpoint().map(|x| Endpoint::Ip(Some(x)));
    }

    fn metadata(&self) -> SocketMetadata {
        self.metadata.clone()
    }

    fn box_clone(&self) -> Box<dyn Socket> {
        Box::new(self.clone())
    }

    fn set_option(
        &self,
        level: SocketOptionsLevel,
        optname: usize,
        optval: &[u8],
    ) -> Result<(), SystemError> {

        let mut socket_set_guard = SOCKET_SET.lock_irqsave();
        let socket = socket_set_guard.get_mut::<tcp::Socket>(self.handles[0].smoltcp_handle().unwrap());

        if level == SocketOptionsLevel::SOL_SOCKET {
            return self.sk_setsockopt(socket, level, PosixSocketOption::try_from(optname as i32)?, optval);
        }

        if level != SocketOptionsLevel::SOL_TCP {
            return self.do_ip_setsockopt(level, optname, optval);
        }
        return self.do_tcp_setsockopt(socket, level, optname, optval);
    }

    fn socket_handle(&self) -> GlobalSocketHandle {
        // debug!("tcp socket:socket_handle, socket'len={}",self.handle.len());

        *self.handles.first().unwrap()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}
