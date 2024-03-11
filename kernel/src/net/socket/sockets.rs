use alloc::{boxed::Box, sync::Arc, vec::Vec};
use smoltcp::{
    iface::SocketHandle,
    socket::{raw, tcp, udp},
    wire,
};
use system_error::SystemError;

use crate::{
    driver::net::NetDriver,
    kerror, kwarn,
    libs::{rwlock::RwLock, spinlock::SpinLock},
    net::{
        event_poll::EPollEventType, net_core::poll_ifaces, Endpoint, Protocol, ShutdownType,
        NET_DRIVERS,
    },
};

use super::{
    GlobalSocketHandle, Socket, SocketHandleItem, SocketMetadata, SocketOptions, SocketPollMethod,
    SocketType, SocketpairOps, HANDLE_MAP, PORT_MANAGER, SOCKET_SET,
};

/// @brief 表示原始的socket。原始套接字绕过传输层协议（如 TCP 或 UDP）并提供对网络层协议（如 IP）的直接访问。
///
/// ref: https://man7.org/linux/man-pages/man7/raw.7.html
#[derive(Debug, Clone)]
pub struct RawSocket {
    handle: Arc<GlobalSocketHandle>,
    /// 用户发送的数据包是否包含了IP头.
    /// 如果是true，用户发送的数据包，必须包含IP头。（即用户要自行设置IP头+数据）
    /// 如果是false，用户发送的数据包，不包含IP头。（即用户只要设置数据）
    header_included: bool,
    /// socket的metadata
    metadata: SocketMetadata,
}

impl RawSocket {
    /// 元数据的缓冲区的大小
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的接收缓冲区的大小 receive
    pub const DEFAULT_RX_BUF_SIZE: usize = 64 * 1024;
    /// 默认的发送缓冲区的大小 transmiss
    pub const DEFAULT_TX_BUF_SIZE: usize = 64 * 1024;

    /// @brief 创建一个原始的socket
    ///
    /// @param protocol 协议号
    /// @param options socket的选项
    ///
    /// @return 返回创建的原始的socket
    pub fn new(protocol: Protocol, options: SocketOptions) -> Self {
        let rx_buffer = raw::PacketBuffer::new(
            vec![raw::PacketMetadata::EMPTY; Self::DEFAULT_METADATA_BUF_SIZE],
            vec![0; Self::DEFAULT_RX_BUF_SIZE],
        );
        let tx_buffer = raw::PacketBuffer::new(
            vec![raw::PacketMetadata::EMPTY; Self::DEFAULT_METADATA_BUF_SIZE],
            vec![0; Self::DEFAULT_TX_BUF_SIZE],
        );
        let protocol: u8 = protocol.into();
        let socket = raw::Socket::new(
            wire::IpVersion::Ipv4,
            wire::IpProtocol::from(protocol),
            rx_buffer,
            tx_buffer,
        );

        // 把socket添加到socket集合中，并得到socket的句柄
        let handle: Arc<GlobalSocketHandle> =
            GlobalSocketHandle::new(SOCKET_SET.lock_irqsave().add(socket));

        let metadata = SocketMetadata::new(
            SocketType::RawSocket,
            Self::DEFAULT_RX_BUF_SIZE,
            Self::DEFAULT_TX_BUF_SIZE,
            Self::DEFAULT_METADATA_BUF_SIZE,
            options,
        );

        return Self {
            handle,
            header_included: false,
            metadata,
        };
    }
}

impl Socket for RawSocket {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn read(&mut self, buf: &mut [u8]) -> (Result<usize, SystemError>, Endpoint) {
        poll_ifaces();
        loop {
            // 如何优化这里？
            let mut socket_set_guard = SOCKET_SET.lock_irqsave();
            let socket = socket_set_guard.get_mut::<raw::Socket>(self.handle.0);

            match socket.recv_slice(buf) {
                Ok(len) => {
                    let packet = wire::Ipv4Packet::new_unchecked(buf);
                    return (
                        Ok(len),
                        Endpoint::Ip(Some(wire::IpEndpoint {
                            addr: wire::IpAddress::Ipv4(packet.src_addr()),
                            port: 0,
                        })),
                    );
                }
                Err(raw::RecvError::Exhausted) => {
                    if !self.metadata.options.contains(SocketOptions::BLOCK) {
                        // 如果是非阻塞的socket，就返回错误
                        return (Err(SystemError::EAGAIN_OR_EWOULDBLOCK), Endpoint::Ip(None));
                    }
                }
            }
            drop(socket_set_guard);
            SocketHandleItem::sleep(
                self.socket_handle(),
                EPollEventType::EPOLLIN.bits() as u64,
                HANDLE_MAP.read_irqsave(),
            );
        }
    }

    fn write(&self, buf: &[u8], to: Option<Endpoint>) -> Result<usize, SystemError> {
        // 如果用户发送的数据包，包含IP头，则直接发送
        if self.header_included {
            let mut socket_set_guard = SOCKET_SET.lock_irqsave();
            let socket = socket_set_guard.get_mut::<raw::Socket>(self.handle.0);
            match socket.send_slice(buf) {
                Ok(_) => {
                    return Ok(buf.len());
                }
                Err(raw::SendError::BufferFull) => {
                    return Err(SystemError::ENOBUFS);
                }
            }
        } else {
            // 如果用户发送的数据包，不包含IP头，则需要自己构造IP头

            if let Some(Endpoint::Ip(Some(endpoint))) = to {
                let mut socket_set_guard = SOCKET_SET.lock_irqsave();
                let socket: &mut raw::Socket =
                    socket_set_guard.get_mut::<raw::Socket>(self.handle.0);

                // 暴力解决方案：只考虑0号网卡。 TODO：考虑多网卡的情况！！！
                let iface = NET_DRIVERS.read_irqsave().get(&0).unwrap().clone();

                // 构造IP头
                let ipv4_src_addr: Option<wire::Ipv4Address> =
                    iface.inner_iface().lock().ipv4_addr();
                if ipv4_src_addr.is_none() {
                    return Err(SystemError::ENETUNREACH);
                }
                let ipv4_src_addr = ipv4_src_addr.unwrap();

                if let wire::IpAddress::Ipv4(ipv4_dst) = endpoint.addr {
                    let len = buf.len();

                    // 创建20字节的IPv4头部
                    let mut buffer: Vec<u8> = vec![0u8; len + 20];
                    let mut packet: wire::Ipv4Packet<&mut Vec<u8>> =
                        wire::Ipv4Packet::new_unchecked(&mut buffer);

                    // 封装ipv4 header
                    packet.set_version(4);
                    packet.set_header_len(20);
                    packet.set_total_len((20 + len) as u16);
                    packet.set_src_addr(ipv4_src_addr);
                    packet.set_dst_addr(ipv4_dst);

                    // 设置ipv4 header的protocol字段
                    packet.set_next_header(socket.ip_protocol());

                    // 获取IP数据包的负载字段
                    let payload: &mut [u8] = packet.payload_mut();
                    payload.copy_from_slice(buf);

                    // 填充checksum字段
                    packet.fill_checksum();

                    // 发送数据包
                    socket.send_slice(&buffer).unwrap();

                    iface.poll(&mut socket_set_guard).ok();

                    drop(socket_set_guard);
                    return Ok(len);
                } else {
                    kwarn!("Unsupport Ip protocol type!");
                    return Err(SystemError::EINVAL);
                }
            } else {
                // 如果没有指定目的地址，则返回错误
                return Err(SystemError::ENOTCONN);
            }
        }
    }

    fn connect(&mut self, _endpoint: Endpoint) -> Result<(), SystemError> {
        Ok(())
    }

    fn metadata(&self) -> Result<SocketMetadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn box_clone(&self) -> Box<dyn Socket> {
        return Box::new(self.clone());
    }

    fn socket_handle(&self) -> SocketHandle {
        self.handle.0
    }
}

/// @brief 表示udp socket
///
/// https://man7.org/linux/man-pages/man7/udp.7.html
#[derive(Debug, Clone)]
pub struct UdpSocket {
    pub handle: Arc<GlobalSocketHandle>,
    remote_endpoint: Option<Endpoint>, // 记录远程endpoint提供给connect()， 应该使用IP地址。
    metadata: SocketMetadata,
}

impl UdpSocket {
    /// 元数据的缓冲区的大小
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的接收缓冲区的大小 receive
    pub const DEFAULT_RX_BUF_SIZE: usize = 64 * 1024;
    /// 默认的发送缓冲区的大小 transmiss
    pub const DEFAULT_TX_BUF_SIZE: usize = 64 * 1024;

    /// @brief 创建一个udp的socket
    ///
    /// @param options socket的选项
    ///
    /// @return 返回创建的udp的socket
    pub fn new(options: SocketOptions) -> Self {
        let rx_buffer = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; Self::DEFAULT_METADATA_BUF_SIZE],
            vec![0; Self::DEFAULT_RX_BUF_SIZE],
        );
        let tx_buffer = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; Self::DEFAULT_METADATA_BUF_SIZE],
            vec![0; Self::DEFAULT_TX_BUF_SIZE],
        );
        let socket = udp::Socket::new(rx_buffer, tx_buffer);

        // 把socket添加到socket集合中，并得到socket的句柄
        let handle: Arc<GlobalSocketHandle> =
            GlobalSocketHandle::new(SOCKET_SET.lock_irqsave().add(socket));

        let metadata = SocketMetadata::new(
            SocketType::UdpSocket,
            Self::DEFAULT_RX_BUF_SIZE,
            Self::DEFAULT_TX_BUF_SIZE,
            Self::DEFAULT_METADATA_BUF_SIZE,
            options,
        );

        return Self {
            handle,
            remote_endpoint: None,
            metadata,
        };
    }

    fn do_bind(&self, socket: &mut udp::Socket, endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(Some(ip)) = endpoint {
            // 检测端口是否已被占用
            PORT_MANAGER.bind_port(self.metadata.socket_type, ip.port, self.handle.clone())?;

            let bind_res = if ip.addr.is_unspecified() {
                socket.bind(ip.port)
            } else {
                socket.bind(ip)
            };

            match bind_res {
                Ok(()) => return Ok(()),
                Err(_) => return Err(SystemError::EINVAL),
            }
        } else {
            return Err(SystemError::EINVAL);
        }
    }
}

impl Socket for UdpSocket {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    /// @brief 在read函数执行之前，请先bind到本地的指定端口
    fn read(&mut self, buf: &mut [u8]) -> (Result<usize, SystemError>, Endpoint) {
        loop {
            // kdebug!("Wait22 to Read");
            poll_ifaces();
            let mut socket_set_guard = SOCKET_SET.lock_irqsave();
            let socket = socket_set_guard.get_mut::<udp::Socket>(self.handle.0);

            // kdebug!("Wait to Read");

            if socket.can_recv() {
                if let Ok((size, remote_endpoint)) = socket.recv_slice(buf) {
                    drop(socket_set_guard);
                    poll_ifaces();
                    return (Ok(size), Endpoint::Ip(Some(remote_endpoint)));
                }
            } else {
                // 如果socket没有连接，则忙等
                // return (Err(SystemError::ENOTCONN), Endpoint::Ip(None));
            }
            drop(socket_set_guard);
            SocketHandleItem::sleep(
                self.socket_handle(),
                EPollEventType::EPOLLIN.bits() as u64,
                HANDLE_MAP.read_irqsave(),
            );
        }
    }

    fn write(&self, buf: &[u8], to: Option<Endpoint>) -> Result<usize, SystemError> {
        // kdebug!("udp to send: {:?}, len={}", to, buf.len());
        let remote_endpoint: &wire::IpEndpoint = {
            if let Some(Endpoint::Ip(Some(ref endpoint))) = to {
                endpoint
            } else if let Some(Endpoint::Ip(Some(ref endpoint))) = self.remote_endpoint {
                endpoint
            } else {
                return Err(SystemError::ENOTCONN);
            }
        };
        // kdebug!("udp write: remote = {:?}", remote_endpoint);

        let mut socket_set_guard = SOCKET_SET.lock_irqsave();
        let socket = socket_set_guard.get_mut::<udp::Socket>(self.handle.0);
        // kdebug!("is open()={}", socket.is_open());
        // kdebug!("socket endpoint={:?}", socket.endpoint());
        if socket.endpoint().port == 0 {
            let temp_port = PORT_MANAGER.get_ephemeral_port(self.metadata.socket_type)?;

            let local_ep = match remote_endpoint.addr {
                // 远程remote endpoint使用什么协议，发送的时候使用的协议是一样的吧
                // 否则就用 self.endpoint().addr.unwrap()
                wire::IpAddress::Ipv4(_) => Endpoint::Ip(Some(wire::IpEndpoint::new(
                    wire::IpAddress::Ipv4(wire::Ipv4Address::UNSPECIFIED),
                    temp_port,
                ))),
                wire::IpAddress::Ipv6(_) => Endpoint::Ip(Some(wire::IpEndpoint::new(
                    wire::IpAddress::Ipv6(wire::Ipv6Address::UNSPECIFIED),
                    temp_port,
                ))),
            };
            // kdebug!("udp write: local_ep = {:?}", local_ep);
            self.do_bind(socket, local_ep)?;
        }
        // kdebug!("is open()={}", socket.is_open());
        if socket.can_send() {
            // kdebug!("udp write: can send");
            match socket.send_slice(buf, *remote_endpoint) {
                Ok(()) => {
                    // kdebug!("udp write: send ok");
                    drop(socket_set_guard);
                    poll_ifaces();
                    return Ok(buf.len());
                }
                Err(_) => {
                    // kdebug!("udp write: send err");
                    return Err(SystemError::ENOBUFS);
                }
            }
        } else {
            // kdebug!("udp write: can not send");
            return Err(SystemError::ENOBUFS);
        };
    }

    fn bind(&mut self, endpoint: Endpoint) -> Result<(), SystemError> {
        let mut sockets = SOCKET_SET.lock_irqsave();
        let socket = sockets.get_mut::<udp::Socket>(self.handle.0);
        // kdebug!("UDP Bind to {:?}", endpoint);
        return self.do_bind(socket, endpoint);
    }

    fn poll(&self) -> EPollEventType {
        let sockets = SOCKET_SET.lock_irqsave();
        let socket = sockets.get::<udp::Socket>(self.handle.0);

        return SocketPollMethod::udp_poll(
            socket,
            HANDLE_MAP
                .read_irqsave()
                .get(&self.socket_handle())
                .unwrap()
                .shutdown_type(),
        );
    }

    fn connect(&mut self, endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(_) = endpoint {
            self.remote_endpoint = Some(endpoint);
            return Ok(());
        } else {
            return Err(SystemError::EINVAL);
        };
    }

    fn ioctl(
        &self,
        _cmd: usize,
        _arg0: usize,
        _arg1: usize,
        _arg2: usize,
    ) -> Result<usize, SystemError> {
        todo!()
    }

    fn metadata(&self) -> Result<SocketMetadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn box_clone(&self) -> Box<dyn Socket> {
        return Box::new(self.clone());
    }

    fn endpoint(&self) -> Option<Endpoint> {
        let sockets = SOCKET_SET.lock_irqsave();
        let socket = sockets.get::<udp::Socket>(self.handle.0);
        let listen_endpoint = socket.endpoint();

        if listen_endpoint.port == 0 {
            return None;
        } else {
            // 如果listen_endpoint的address是None，意味着“监听所有的地址”。
            // 这里假设所有的地址都是ipv4
            // TODO: 支持ipv6
            let result = wire::IpEndpoint::new(
                listen_endpoint
                    .addr
                    .unwrap_or(wire::IpAddress::v4(0, 0, 0, 0)),
                listen_endpoint.port,
            );
            return Some(Endpoint::Ip(Some(result)));
        }
    }

    fn peer_endpoint(&self) -> Option<Endpoint> {
        return self.remote_endpoint.clone();
    }

    fn socket_handle(&self) -> SocketHandle {
        self.handle.0
    }
}

/// @brief 表示 tcp socket
///
/// https://man7.org/linux/man-pages/man7/tcp.7.html
#[derive(Debug, Clone)]
pub struct TcpSocket {
    handle: Arc<GlobalSocketHandle>,
    local_endpoint: Option<wire::IpEndpoint>, // save local endpoint for bind()
    is_listening: bool,
    metadata: SocketMetadata,
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
        let rx_buffer = tcp::SocketBuffer::new(vec![0; Self::DEFAULT_RX_BUF_SIZE]);
        let tx_buffer = tcp::SocketBuffer::new(vec![0; Self::DEFAULT_TX_BUF_SIZE]);
        let socket = tcp::Socket::new(rx_buffer, tx_buffer);

        // 把socket添加到socket集合中，并得到socket的句柄
        let handle: Arc<GlobalSocketHandle> =
            GlobalSocketHandle::new(SOCKET_SET.lock_irqsave().add(socket));

        let metadata = SocketMetadata::new(
            SocketType::TcpSocket,
            Self::DEFAULT_RX_BUF_SIZE,
            Self::DEFAULT_TX_BUF_SIZE,
            Self::DEFAULT_METADATA_BUF_SIZE,
            options,
        );

        return Self {
            handle,
            local_endpoint: None,
            is_listening: false,
            metadata,
        };
    }
    fn do_listen(
        &mut self,
        socket: &mut tcp::Socket,
        local_endpoint: wire::IpEndpoint,
    ) -> Result<(), SystemError> {
        let listen_result = if local_endpoint.addr.is_unspecified() {
            // kdebug!("Tcp Socket Listen on port {}", local_endpoint.port);
            socket.listen(local_endpoint.port)
        } else {
            // kdebug!("Tcp Socket Listen on {local_endpoint}");
            socket.listen(local_endpoint)
        };
        // TODO: 增加端口占用检查
        return match listen_result {
            Ok(()) => {
                // kdebug!(
                //     "Tcp Socket Listen on {local_endpoint}, open?:{}",
                //     socket.is_open()
                // );
                self.is_listening = true;

                Ok(())
            }
            Err(_) => Err(SystemError::EINVAL),
        };
    }
}

impl Socket for TcpSocket {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn read(&mut self, buf: &mut [u8]) -> (Result<usize, SystemError>, Endpoint) {
        if HANDLE_MAP
            .read_irqsave()
            .get(&self.socket_handle())
            .unwrap()
            .shutdown_type()
            .contains(ShutdownType::RCV_SHUTDOWN)
        {
            return (Err(SystemError::ENOTCONN), Endpoint::Ip(None));
        }
        // kdebug!("tcp socket: read, buf len={}", buf.len());

        loop {
            poll_ifaces();
            let mut socket_set_guard = SOCKET_SET.lock_irqsave();
            let socket = socket_set_guard.get_mut::<tcp::Socket>(self.handle.0);

            // 如果socket已经关闭，返回错误
            if !socket.is_active() {
                // kdebug!("Tcp Socket Read Error, socket is closed");
                return (Err(SystemError::ENOTCONN), Endpoint::Ip(None));
            }

            if socket.may_recv() {
                let recv_res = socket.recv_slice(buf);

                if let Ok(size) = recv_res {
                    if size > 0 {
                        let endpoint = if let Some(p) = socket.remote_endpoint() {
                            p
                        } else {
                            return (Err(SystemError::ENOTCONN), Endpoint::Ip(None));
                        };

                        drop(socket_set_guard);
                        poll_ifaces();
                        return (Ok(size), Endpoint::Ip(Some(endpoint)));
                    }
                } else {
                    let err = recv_res.unwrap_err();
                    match err {
                        tcp::RecvError::InvalidState => {
                            kwarn!("Tcp Socket Read Error, InvalidState");
                            return (Err(SystemError::ENOTCONN), Endpoint::Ip(None));
                        }
                        tcp::RecvError::Finished => {
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
                }
            } else {
                return (Err(SystemError::ENOTCONN), Endpoint::Ip(None));
            }
            drop(socket_set_guard);
            SocketHandleItem::sleep(
                self.socket_handle(),
                EPollEventType::EPOLLIN.bits() as u64,
                HANDLE_MAP.read_irqsave(),
            );
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
        let mut socket_set_guard = SOCKET_SET.lock_irqsave();
        let socket = socket_set_guard.get_mut::<tcp::Socket>(self.handle.0);

        if socket.is_open() {
            if socket.can_send() {
                match socket.send_slice(buf) {
                    Ok(size) => {
                        drop(socket_set_guard);
                        poll_ifaces();
                        return Ok(size);
                    }
                    Err(e) => {
                        kerror!("Tcp Socket Write Error {e:?}");
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
        let mut socket_set_guard = SOCKET_SET.lock_irqsave();
        let socket = socket_set_guard.get_mut::<tcp::Socket>(self.handle.0);

        return SocketPollMethod::tcp_poll(
            socket,
            HANDLE_MAP
                .read_irqsave()
                .get(&self.socket_handle())
                .unwrap()
                .shutdown_type(),
        );
    }

    fn connect(&mut self, endpoint: Endpoint) -> Result<(), SystemError> {
        let mut sockets = SOCKET_SET.lock_irqsave();
        let socket = sockets.get_mut::<tcp::Socket>(self.handle.0);

        if let Endpoint::Ip(Some(ip)) = endpoint {
            let temp_port = PORT_MANAGER.get_ephemeral_port(self.metadata.socket_type)?;
            // 检测端口是否被占用
            PORT_MANAGER.bind_port(self.metadata.socket_type, temp_port, self.handle.clone())?;

            // kdebug!("temp_port: {}", temp_port);
            let iface: Arc<dyn NetDriver> = NET_DRIVERS.write_irqsave().get(&0).unwrap().clone();
            let mut inner_iface = iface.inner_iface().lock();
            // kdebug!("to connect: {ip:?}");

            match socket.connect(inner_iface.context(), ip, temp_port) {
                Ok(()) => {
                    // avoid deadlock
                    drop(inner_iface);
                    drop(iface);
                    drop(sockets);
                    loop {
                        poll_ifaces();
                        let mut sockets = SOCKET_SET.lock_irqsave();
                        let socket = sockets.get_mut::<tcp::Socket>(self.handle.0);

                        match socket.state() {
                            tcp::State::Established => {
                                return Ok(());
                            }
                            tcp::State::SynSent => {
                                drop(sockets);
                                SocketHandleItem::sleep(
                                    self.socket_handle(),
                                    Self::CAN_CONNECT,
                                    HANDLE_MAP.read_irqsave(),
                                );
                            }
                            _ => {
                                return Err(SystemError::ECONNREFUSED);
                            }
                        }
                    }
                }
                Err(e) => {
                    // kerror!("Tcp Socket Connect Error {e:?}");
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
    /// @param backlog 未处理的连接队列的最大长度. 由于smoltcp不支持backlog，所以这个参数目前无效
    fn listen(&mut self, _backlog: usize) -> Result<(), SystemError> {
        if self.is_listening {
            return Ok(());
        }

        let local_endpoint = self.local_endpoint.ok_or(SystemError::EINVAL)?;
        let mut sockets = SOCKET_SET.lock_irqsave();
        let socket = sockets.get_mut::<tcp::Socket>(self.handle.0);

        if socket.is_listening() {
            // kdebug!("Tcp Socket is already listening on {local_endpoint}");
            return Ok(());
        }
        // kdebug!("Tcp Socket  before listen, open={}", socket.is_open());
        return self.do_listen(socket, local_endpoint);
    }

    fn bind(&mut self, endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(Some(mut ip)) = endpoint {
            if ip.port == 0 {
                ip.port = PORT_MANAGER.get_ephemeral_port(self.metadata.socket_type)?;
            }

            // 检测端口是否已被占用
            PORT_MANAGER.bind_port(self.metadata.socket_type, ip.port, self.handle.clone())?;

            self.local_endpoint = Some(ip);
            self.is_listening = false;
            return Ok(());
        }
        return Err(SystemError::EINVAL);
    }

    fn shutdown(&mut self, shutdown_type: super::ShutdownType) -> Result<(), SystemError> {
        // TODO：目前只是在表层判断，对端不知晓，后续需使用tcp实现
        HANDLE_MAP
            .write_irqsave()
            .get_mut(&self.socket_handle())
            .unwrap()
            .shutdown_type = RwLock::new(shutdown_type);
        return Ok(());
    }

    fn accept(&mut self) -> Result<(Box<dyn Socket>, Endpoint), SystemError> {
        let endpoint = self.local_endpoint.ok_or(SystemError::EINVAL)?;
        loop {
            // kdebug!("tcp accept: poll_ifaces()");
            poll_ifaces();

            let mut sockets = SOCKET_SET.lock_irqsave();

            let socket = sockets.get_mut::<tcp::Socket>(self.handle.0);

            if socket.is_active() {
                // kdebug!("tcp accept: socket.is_active()");
                let remote_ep = socket.remote_endpoint().ok_or(SystemError::ENOTCONN)?;

                let new_socket = {
                    // Initialize the TCP socket's buffers.
                    let rx_buffer = tcp::SocketBuffer::new(vec![0; Self::DEFAULT_RX_BUF_SIZE]);
                    let tx_buffer = tcp::SocketBuffer::new(vec![0; Self::DEFAULT_TX_BUF_SIZE]);
                    // The new TCP socket used for sending and receiving data.
                    let mut tcp_socket = tcp::Socket::new(rx_buffer, tx_buffer);
                    self.do_listen(&mut tcp_socket, endpoint)
                        .expect("do_listen failed");

                    // tcp_socket.listen(endpoint).unwrap();

                    // 之所以把old_handle存入new_socket, 是因为当前时刻，smoltcp已经把old_handle对应的socket与远程的endpoint关联起来了
                    // 因此需要再为当前的socket分配一个新的handle
                    let new_handle = GlobalSocketHandle::new(sockets.add(tcp_socket));
                    let old_handle = ::core::mem::replace(&mut self.handle, new_handle.clone());

                    // 更新端口与 handle 的绑定
                    if let Some(Endpoint::Ip(Some(ip))) = self.endpoint() {
                        PORT_MANAGER.unbind_port(self.metadata.socket_type, ip.port)?;
                        PORT_MANAGER.bind_port(
                            self.metadata.socket_type,
                            ip.port,
                            new_handle.clone(),
                        )?;
                    }

                    let metadata = SocketMetadata::new(
                        SocketType::TcpSocket,
                        Self::DEFAULT_TX_BUF_SIZE,
                        Self::DEFAULT_RX_BUF_SIZE,
                        Self::DEFAULT_METADATA_BUF_SIZE,
                        self.metadata.options,
                    );

                    let new_socket = Box::new(TcpSocket {
                        handle: old_handle.clone(),
                        local_endpoint: self.local_endpoint,
                        is_listening: false,
                        metadata,
                    });

                    // 更新handle表
                    let mut handle_guard = HANDLE_MAP.write_irqsave();
                    // 先删除原来的
                    let item = handle_guard.remove(&old_handle.0).unwrap();
                    // 按照smoltcp行为，将新的handle绑定到原来的item
                    handle_guard.insert(new_handle.0, item);
                    let new_item = SocketHandleItem::from_socket(&new_socket);
                    // 插入新的item
                    handle_guard.insert(old_handle.0, new_item);

                    new_socket
                };
                // kdebug!("tcp accept: new socket: {:?}", new_socket);
                drop(sockets);
                poll_ifaces();

                return Ok((new_socket, Endpoint::Ip(Some(remote_ep))));
            }
            drop(sockets);

            SocketHandleItem::sleep(
                self.socket_handle(),
                Self::CAN_ACCPET,
                HANDLE_MAP.read_irqsave(),
            );
        }
    }

    fn endpoint(&self) -> Option<Endpoint> {
        let mut result: Option<Endpoint> =
            self.local_endpoint.clone().map(|x| Endpoint::Ip(Some(x)));

        if result.is_none() {
            let sockets = SOCKET_SET.lock_irqsave();
            let socket = sockets.get::<tcp::Socket>(self.handle.0);
            if let Some(ep) = socket.local_endpoint() {
                result = Some(Endpoint::Ip(Some(ep)));
            }
        }
        return result;
    }

    fn peer_endpoint(&self) -> Option<Endpoint> {
        let sockets = SOCKET_SET.lock_irqsave();
        let socket = sockets.get::<tcp::Socket>(self.handle.0);
        return socket.remote_endpoint().map(|x| Endpoint::Ip(Some(x)));
    }

    fn metadata(&self) -> Result<SocketMetadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn box_clone(&self) -> Box<dyn Socket> {
        return Box::new(self.clone());
    }

    fn socket_handle(&self) -> SocketHandle {
        self.handle.0
    }
}

/// # 表示 seqpacket socket
#[derive(Debug, Clone)]
#[cast_to(Socket)]
pub struct SeqpacketSocket {
    metadata: SocketMetadata,
    buffer: Arc<SpinLock<Vec<u8>>>,
    peer_buffer: Option<Arc<SpinLock<Vec<u8>>>>,
}

impl SeqpacketSocket {
    /// 默认的元数据缓冲区大小
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的缓冲区大小
    pub const DEFAULT_BUF_SIZE: usize = 64 * 1024;

    /// # 创建一个seqpacket的socket
    ///
    /// ## 参数
    /// - `options`: socket的选项
    pub fn new(options: SocketOptions) -> Self {
        let buffer = Vec::with_capacity(Self::DEFAULT_BUF_SIZE);

        let metadata = SocketMetadata::new(
            SocketType::SeqpacketSocket,
            Self::DEFAULT_BUF_SIZE,
            0,
            Self::DEFAULT_METADATA_BUF_SIZE,
            options,
        );

        return Self {
            metadata,
            buffer: Arc::new(SpinLock::new(buffer)),
            peer_buffer: None,
        };
    }

    fn buffer(&self) -> Arc<SpinLock<Vec<u8>>> {
        self.buffer.clone()
    }

    fn set_peer_buffer(&mut self, peer_buffer: Arc<SpinLock<Vec<u8>>>) {
        self.peer_buffer = Some(peer_buffer);
    }
}

impl Socket for SeqpacketSocket {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn read(&mut self, buf: &mut [u8]) -> (Result<usize, SystemError>, Endpoint) {
        let buffer = self.buffer.lock_irqsave();

        let len = core::cmp::min(buf.len(), buffer.len());
        buf[..len].copy_from_slice(&buffer[..len]);

        (Ok(len), Endpoint::Unused)
    }

    fn write(&self, buf: &[u8], _to: Option<Endpoint>) -> Result<usize, SystemError> {
        if self.peer_buffer.is_none() {
            kwarn!("SeqpacketSocket is now just for socketpair");
            return Err(SystemError::ENOSYS);
        }

        let binding = self.peer_buffer.clone().unwrap();
        let mut peer_buffer = binding.lock_irqsave();

        let len = buf.len();
        if peer_buffer.capacity() - peer_buffer.len() < len {
            return Err(SystemError::ENOBUFS);
        }
        peer_buffer[..len].copy_from_slice(buf);

        Ok(len)
    }

    fn socketpair_ops(&self) -> Option<&'static dyn SocketpairOps> {
        Some(&SeqpacketSocketpairOps)
    }

    fn metadata(&self) -> Result<SocketMetadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn box_clone(&self) -> Box<dyn Socket> {
        Box::new(self.clone())
    }
}

struct SeqpacketSocketpairOps;

impl SocketpairOps for SeqpacketSocketpairOps {
    fn socketpair(&self, socket0: &mut Box<dyn Socket>, socket1: &mut Box<dyn Socket>) {
        let pair0 = socket0
            .as_mut()
            .as_any_mut()
            .downcast_mut::<SeqpacketSocket>()
            .unwrap();

        let pair1 = socket1
            .as_mut()
            .as_any_mut()
            .downcast_mut::<SeqpacketSocket>()
            .unwrap();
        pair0.set_peer_buffer(pair1.buffer());
        pair1.set_peer_buffer(pair0.buffer());
    }
}
