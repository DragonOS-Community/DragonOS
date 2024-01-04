#![allow(dead_code)]
use alloc::{
    boxed::Box,
    collections::LinkedList,
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use smoltcp::{
    iface::{SocketHandle, SocketSet},
    socket::{
        self, raw,
        tcp::{self, State},
        udp,
    },
    wire,
};
use system_error::SystemError;

use crate::{
    arch::{rand::rand, sched::sched},
    driver::net::NetDriver,
    filesystem::vfs::{syscall::ModeType, FilePrivateData, FileType, IndexNode, Metadata},
    kerror, kwarn,
    libs::{
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::EventWaitQueue,
    },
};

use super::{
    event_poll::{EPollEventType, EPollItem, EventPoll},
    net_core::poll_ifaces,
    Endpoint, Protocol, ShutdownType, Socket, NET_DRIVERS,
};

lazy_static! {
    /// 所有socket的集合
    /// TODO: 优化这里，自己实现SocketSet！！！现在这样的话，不管全局有多少个网卡，每个时间点都只会有1个进程能够访问socket
    pub static ref SOCKET_SET: SpinLock<SocketSet<'static >> = SpinLock::new(SocketSet::new(vec![]));
    /// SocketHandle表，每个SocketHandle对应一个SocketHandleItem，
    /// 注意！：在网卡中断中需要拿到这张表的🔓，在获取读锁时应该确保关中断避免死锁
    pub static ref HANDLE_MAP: RwLock<HashMap<SocketHandle,SocketHandleItem>> = RwLock::new(HashMap::new());
    /// 端口管理器
    pub static ref PORT_MANAGER: PortManager = PortManager::new();
}

#[derive(Debug)]
pub struct SocketHandleItem {
    /// socket元数据
    metadata: SocketMetadata,
    /// shutdown状态
    pub shutdown_type: RwLock<ShutdownType>,
    /// socket的waitqueue
    pub wait_queue: EventWaitQueue,
    /// epitems，考虑写在这是否是最优解？
    pub epitems: SpinLock<LinkedList<Arc<EPollItem>>>,
}

impl SocketHandleItem {
    pub fn new(socket: &Box<dyn Socket>) -> Self {
        Self {
            metadata: socket.metadata().unwrap(),
            shutdown_type: RwLock::new(ShutdownType::empty()),
            wait_queue: EventWaitQueue::new(),
            epitems: SpinLock::new(LinkedList::new()),
        }
    }

    pub fn from_socket<A: Socket>(socket: &Box<A>) -> Self {
        Self {
            metadata: socket.metadata().unwrap(),
            shutdown_type: RwLock::new(ShutdownType::empty()),
            wait_queue: EventWaitQueue::new(),
            epitems: SpinLock::new(LinkedList::new()),
        }
    }

    /// ### 在socket的等待队列上睡眠
    pub fn sleep(
        socket_handle: SocketHandle,
        events: u64,
        handle_map_guard: RwLockReadGuard<'_, HashMap<SocketHandle, SocketHandleItem>>,
    ) {
        unsafe {
            handle_map_guard
                .get(&socket_handle)
                .unwrap()
                .wait_queue
                .sleep_without_schedule(events)
        };
        drop(handle_map_guard);
        sched();
    }

    pub fn shutdown_type(&self) -> ShutdownType {
        self.shutdown_type.read().clone()
    }

    pub fn shutdown_type_writer(&mut self) -> RwLockWriteGuard<ShutdownType> {
        self.shutdown_type.write_irqsave()
    }

    pub fn add_epoll(&mut self, epitem: Arc<EPollItem>) {
        self.epitems.lock_irqsave().push_back(epitem)
    }

    pub fn remove_epoll(&mut self, epoll: &Weak<SpinLock<EventPoll>>) -> Result<(), SystemError> {
        let is_remove = !self
            .epitems
            .lock_irqsave()
            .extract_if(|x| x.epoll().ptr_eq(epoll))
            .collect::<Vec<_>>()
            .is_empty();

        if is_remove {
            return Ok(());
        }

        Err(SystemError::ENOENT)
    }
}

/// @brief TCP 和 UDP 的端口管理器。
/// 如果 TCP/UDP 的 socket 绑定了某个端口，它会在对应的表中记录，以检测端口冲突。
pub struct PortManager {
    // TCP 端口记录表
    tcp_port_table: SpinLock<HashMap<u16, Arc<GlobalSocketHandle>>>,
    // UDP 端口记录表
    udp_port_table: SpinLock<HashMap<u16, Arc<GlobalSocketHandle>>>,
}

impl PortManager {
    pub fn new() -> Self {
        return Self {
            tcp_port_table: SpinLock::new(HashMap::new()),
            udp_port_table: SpinLock::new(HashMap::new()),
        };
    }

    /// @brief 自动分配一个相对应协议中未被使用的PORT，如果动态端口均已被占用，返回错误码 EADDRINUSE
    pub fn get_ephemeral_port(&self, socket_type: SocketType) -> Result<u16, SystemError> {
        // TODO: selects non-conflict high port

        static mut EPHEMERAL_PORT: u16 = 0;
        unsafe {
            if EPHEMERAL_PORT == 0 {
                EPHEMERAL_PORT = (49152 + rand() % (65536 - 49152)) as u16;
            }
        }

        let mut remaining = 65536 - 49152; // 剩余尝试分配端口次数
        let mut port: u16;
        while remaining > 0 {
            unsafe {
                if EPHEMERAL_PORT == 65535 {
                    EPHEMERAL_PORT = 49152;
                } else {
                    EPHEMERAL_PORT = EPHEMERAL_PORT + 1;
                }
                port = EPHEMERAL_PORT;
            }

            // 使用 ListenTable 检查端口是否被占用
            let listen_table_guard = match socket_type {
                SocketType::UdpSocket => self.udp_port_table.lock(),
                SocketType::TcpSocket => self.tcp_port_table.lock(),
                SocketType::RawSocket => panic!("RawSocket cann't get a port"),
            };
            if let None = listen_table_guard.get(&port) {
                drop(listen_table_guard);
                return Ok(port);
            }
            remaining -= 1;
        }
        return Err(SystemError::EADDRINUSE);
    }

    /// @brief 检测给定端口是否已被占用，如果未被占用则在 TCP/UDP 对应的表中记录
    ///
    /// TODO: 增加支持端口复用的逻辑
    pub fn bind_port(
        &self,
        socket_type: SocketType,
        port: u16,
        handle: Arc<GlobalSocketHandle>,
    ) -> Result<(), SystemError> {
        if port > 0 {
            let mut listen_table_guard = match socket_type {
                SocketType::UdpSocket => self.udp_port_table.lock(),
                SocketType::TcpSocket => self.tcp_port_table.lock(),
                SocketType::RawSocket => panic!("RawSocket cann't bind a port"),
            };
            match listen_table_guard.get(&port) {
                Some(_) => return Err(SystemError::EADDRINUSE),
                None => listen_table_guard.insert(port, handle),
            };
            drop(listen_table_guard);
        }
        return Ok(());
    }

    /// @brief 在对应的端口记录表中将端口和 socket 解绑
    pub fn unbind_port(&self, socket_type: SocketType, port: u16) -> Result<(), SystemError> {
        let mut listen_table_guard = match socket_type {
            SocketType::UdpSocket => self.udp_port_table.lock(),
            SocketType::TcpSocket => self.tcp_port_table.lock(),
            SocketType::RawSocket => return Ok(()),
        };
        listen_table_guard.remove(&port);
        drop(listen_table_guard);
        return Ok(());
    }
}

/* For setsockopt(2) */
// See: linux-5.19.10/include/uapi/asm-generic/socket.h#9
pub const SOL_SOCKET: u8 = 1;

/// @brief socket的句柄管理组件。
/// 它在smoltcp的SocketHandle上封装了一层，增加更多的功能。
/// 比如，在socket被关闭时，自动释放socket的资源，通知系统的其他组件。
#[derive(Debug)]
pub struct GlobalSocketHandle(SocketHandle);

impl GlobalSocketHandle {
    pub fn new(handle: SocketHandle) -> Arc<Self> {
        return Arc::new(Self(handle));
    }
}

impl Clone for GlobalSocketHandle {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl Drop for GlobalSocketHandle {
    fn drop(&mut self) {
        let mut socket_set_guard = SOCKET_SET.lock_irqsave();
        socket_set_guard.remove(self.0); // 删除的时候，会发送一条FINISH的信息？
        drop(socket_set_guard);
        poll_ifaces();
    }
}

/// @brief socket的类型
#[derive(Debug, Clone, Copy)]
pub enum SocketType {
    /// 原始的socket
    RawSocket,
    /// 用于Tcp通信的 Socket
    TcpSocket,
    /// 用于Udp通信的 Socket
    UdpSocket,
}

bitflags! {
    /// @brief socket的选项
    #[derive(Default)]
    pub struct SocketOptions: u32 {
        /// 是否阻塞
        const BLOCK = 1 << 0;
        /// 是否允许广播
        const BROADCAST = 1 << 1;
        /// 是否允许多播
        const MULTICAST = 1 << 2;
        /// 是否允许重用地址
        const REUSEADDR = 1 << 3;
        /// 是否允许重用端口
        const REUSEPORT = 1 << 4;
    }
}

#[derive(Debug, Clone)]
/// @brief 在trait Socket的metadata函数中返回该结构体供外部使用
pub struct SocketMetadata {
    /// socket的类型
    pub socket_type: SocketType,
    /// 发送缓冲区的大小
    pub send_buf_size: usize,
    /// 接收缓冲区的大小
    pub recv_buf_size: usize,
    /// 元数据的缓冲区的大小
    pub metadata_buf_size: usize,
    /// socket的选项
    pub options: SocketOptions,
}

impl SocketMetadata {
    fn new(
        socket_type: SocketType,
        send_buf_size: usize,
        recv_buf_size: usize,
        metadata_buf_size: usize,
        options: SocketOptions,
    ) -> Self {
        Self {
            socket_type,
            send_buf_size,
            recv_buf_size,
            metadata_buf_size,
            options,
        }
    }
}

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
    /// 默认的发送缓冲区的大小 transmiss
    pub const DEFAULT_RX_BUF_SIZE: usize = 64 * 1024;
    /// 默认的接收缓冲区的大小 receive
    pub const DEFAULT_TX_BUF_SIZE: usize = 64 * 1024;

    /// @brief 创建一个原始的socket
    ///
    /// @param protocol 协议号
    /// @param options socket的选项
    ///
    /// @return 返回创建的原始的socket
    pub fn new(protocol: Protocol, options: SocketOptions) -> Self {
        let tx_buffer = raw::PacketBuffer::new(
            vec![raw::PacketMetadata::EMPTY; Self::DEFAULT_METADATA_BUF_SIZE],
            vec![0; Self::DEFAULT_TX_BUF_SIZE],
        );
        let rx_buffer = raw::PacketBuffer::new(
            vec![raw::PacketMetadata::EMPTY; Self::DEFAULT_METADATA_BUF_SIZE],
            vec![0; Self::DEFAULT_RX_BUF_SIZE],
        );
        let protocol: u8 = protocol.into();
        let socket = raw::Socket::new(
            smoltcp::wire::IpVersion::Ipv4,
            wire::IpProtocol::from(protocol),
            tx_buffer,
            rx_buffer,
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
                        Endpoint::Ip(Some(smoltcp::wire::IpEndpoint {
                            addr: wire::IpAddress::Ipv4(packet.src_addr()),
                            port: 0,
                        })),
                    );
                }
                Err(smoltcp::socket::raw::RecvError::Exhausted) => {
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

    fn write(&self, buf: &[u8], to: Option<super::Endpoint>) -> Result<usize, SystemError> {
        // 如果用户发送的数据包，包含IP头，则直接发送
        if self.header_included {
            let mut socket_set_guard = SOCKET_SET.lock_irqsave();
            let socket = socket_set_guard.get_mut::<raw::Socket>(self.handle.0);
            match socket.send_slice(buf) {
                Ok(_len) => {
                    return Ok(buf.len());
                }
                Err(smoltcp::socket::raw::SendError::BufferFull) => {
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
                let ipv4_src_addr: Option<smoltcp::wire::Ipv4Address> =
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
                    packet.set_next_header(socket.ip_protocol().into());

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

    fn connect(&mut self, _endpoint: super::Endpoint) -> Result<(), SystemError> {
        return Ok(());
    }

    fn metadata(&self) -> Result<SocketMetadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn box_clone(&self) -> alloc::boxed::Box<dyn Socket> {
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
    /// 默认的发送缓冲区的大小 transmiss
    pub const DEFAULT_RX_BUF_SIZE: usize = 64 * 1024;
    /// 默认的接收缓冲区的大小 receive
    pub const DEFAULT_TX_BUF_SIZE: usize = 64 * 1024;

    /// @brief 创建一个原始的socket
    ///
    /// @param protocol 协议号
    /// @param options socket的选项
    ///
    /// @return 返回创建的原始的socket
    pub fn new(options: SocketOptions) -> Self {
        let tx_buffer = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; Self::DEFAULT_METADATA_BUF_SIZE],
            vec![0; Self::DEFAULT_TX_BUF_SIZE],
        );
        let rx_buffer = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; Self::DEFAULT_METADATA_BUF_SIZE],
            vec![0; Self::DEFAULT_RX_BUF_SIZE],
        );
        let socket = udp::Socket::new(tx_buffer, rx_buffer);

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
        };
    }
}

impl Socket for UdpSocket {
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

    fn write(&self, buf: &[u8], to: Option<super::Endpoint>) -> Result<usize, SystemError> {
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
                    smoltcp::wire::IpAddress::Ipv4(wire::Ipv4Address::UNSPECIFIED),
                    temp_port,
                ))),
                wire::IpAddress::Ipv6(_) => Endpoint::Ip(Some(wire::IpEndpoint::new(
                    smoltcp::wire::IpAddress::Ipv6(wire::Ipv6Address::UNSPECIFIED),
                    temp_port,
                ))),
            };
            // kdebug!("udp write: local_ep = {:?}", local_ep);
            self.do_bind(socket, local_ep)?;
        }
        // kdebug!("is open()={}", socket.is_open());
        if socket.can_send() {
            // kdebug!("udp write: can send");
            match socket.send_slice(&buf, *remote_endpoint) {
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

    /// @brief
    fn connect(&mut self, endpoint: super::Endpoint) -> Result<(), SystemError> {
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

    fn box_clone(&self) -> alloc::boxed::Box<dyn Socket> {
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
    /// 默认的发送缓冲区的大小 transmiss
    pub const DEFAULT_RX_BUF_SIZE: usize = 512 * 1024;
    /// 默认的接收缓冲区的大小 receive
    pub const DEFAULT_TX_BUF_SIZE: usize = 512 * 1024;

    /// TcpSocket的特殊事件，用于在事件等待队列上sleep
    pub const CAN_CONNECT: u64 = 1u64 << 63;
    pub const CAN_ACCPET: u64 = 1u64 << 62;

    /// @brief 创建一个原始的socket
    ///
    /// @param protocol 协议号
    /// @param options socket的选项
    ///
    /// @return 返回创建的原始的socket
    pub fn new(options: SocketOptions) -> Self {
        let tx_buffer = tcp::SocketBuffer::new(vec![0; Self::DEFAULT_TX_BUF_SIZE]);
        let rx_buffer = tcp::SocketBuffer::new(vec![0; Self::DEFAULT_RX_BUF_SIZE]);
        let socket = tcp::Socket::new(tx_buffer, rx_buffer);

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
        socket: &mut smoltcp::socket::tcp::Socket,
        local_endpoint: smoltcp::wire::IpEndpoint,
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

    fn write(&self, buf: &[u8], _to: Option<super::Endpoint>) -> Result<usize, SystemError> {
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

            match socket.connect(&mut inner_iface.context(), ip, temp_port) {
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
                        Self::DEFAULT_RX_BUF_SIZE,
                        Self::DEFAULT_TX_BUF_SIZE,
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

    fn box_clone(&self) -> alloc::boxed::Box<dyn Socket> {
        return Box::new(self.clone());
    }

    fn socket_handle(&self) -> SocketHandle {
        self.handle.0
    }
}

/// @brief 地址族的枚举
///
/// 参考：https://code.dragonos.org.cn/xref/linux-5.19.10/include/linux/socket.h#180
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
pub enum AddressFamily {
    /// AF_UNSPEC 表示地址族未指定
    Unspecified = 0,
    /// AF_UNIX 表示Unix域的socket (与AF_LOCAL相同)
    Unix = 1,
    ///  AF_INET 表示IPv4的socket
    INet = 2,
    /// AF_AX25 表示AMPR AX.25的socket
    AX25 = 3,
    /// AF_IPX 表示IPX的socket
    IPX = 4,
    /// AF_APPLETALK 表示Appletalk的socket
    Appletalk = 5,
    /// AF_NETROM 表示AMPR NET/ROM的socket
    Netrom = 6,
    /// AF_BRIDGE 表示多协议桥接的socket
    Bridge = 7,
    /// AF_ATMPVC 表示ATM PVCs的socket
    Atmpvc = 8,
    /// AF_X25 表示X.25的socket
    X25 = 9,
    /// AF_INET6 表示IPv6的socket
    INet6 = 10,
    /// AF_ROSE 表示AMPR ROSE的socket
    Rose = 11,
    /// AF_DECnet Reserved for DECnet project
    Decnet = 12,
    /// AF_NETBEUI Reserved for 802.2LLC project
    Netbeui = 13,
    /// AF_SECURITY 表示Security callback的伪AF
    Security = 14,
    /// AF_KEY 表示Key management API
    Key = 15,
    /// AF_NETLINK 表示Netlink的socket
    Netlink = 16,
    /// AF_PACKET 表示Low level packet interface
    Packet = 17,
    /// AF_ASH 表示Ash
    Ash = 18,
    /// AF_ECONET 表示Acorn Econet
    Econet = 19,
    /// AF_ATMSVC 表示ATM SVCs
    Atmsvc = 20,
    /// AF_RDS 表示Reliable Datagram Sockets
    Rds = 21,
    /// AF_SNA 表示Linux SNA Project
    Sna = 22,
    /// AF_IRDA 表示IRDA sockets
    Irda = 23,
    /// AF_PPPOX 表示PPPoX sockets
    Pppox = 24,
    /// AF_WANPIPE 表示WANPIPE API sockets
    WanPipe = 25,
    /// AF_LLC 表示Linux LLC
    Llc = 26,
    /// AF_IB 表示Native InfiniBand address
    /// 介绍：https://access.redhat.com/documentation/en-us/red_hat_enterprise_linux/9/html-single/configuring_infiniband_and_rdma_networks/index#understanding-infiniband-and-rdma_configuring-infiniband-and-rdma-networks
    Ib = 27,
    /// AF_MPLS 表示MPLS
    Mpls = 28,
    /// AF_CAN 表示Controller Area Network
    Can = 29,
    /// AF_TIPC 表示TIPC sockets
    Tipc = 30,
    /// AF_BLUETOOTH 表示Bluetooth sockets
    Bluetooth = 31,
    /// AF_IUCV 表示IUCV sockets
    Iucv = 32,
    /// AF_RXRPC 表示RxRPC sockets
    Rxrpc = 33,
    /// AF_ISDN 表示mISDN sockets
    Isdn = 34,
    /// AF_PHONET 表示Phonet sockets
    Phonet = 35,
    /// AF_IEEE802154 表示IEEE 802.15.4 sockets
    Ieee802154 = 36,
    /// AF_CAIF 表示CAIF sockets
    Caif = 37,
    /// AF_ALG 表示Algorithm sockets
    Alg = 38,
    /// AF_NFC 表示NFC sockets
    Nfc = 39,
    /// AF_VSOCK 表示vSockets
    Vsock = 40,
    /// AF_KCM 表示Kernel Connection Multiplexor
    Kcm = 41,
    /// AF_QIPCRTR 表示Qualcomm IPC Router
    Qipcrtr = 42,
    /// AF_SMC 表示SMC-R sockets.
    /// reserve number for PF_SMC protocol family that reuses AF_INET address family
    Smc = 43,
    /// AF_XDP 表示XDP sockets
    Xdp = 44,
    /// AF_MCTP 表示Management Component Transport Protocol
    Mctp = 45,
    /// AF_MAX 表示最大的地址族
    Max = 46,
}

impl TryFrom<u16> for AddressFamily {
    type Error = SystemError;
    fn try_from(x: u16) -> Result<Self, Self::Error> {
        use num_traits::FromPrimitive;
        return <Self as FromPrimitive>::from_u16(x).ok_or_else(|| SystemError::EINVAL);
    }
}

/// @brief posix套接字类型的枚举(这些值与linux内核中的值一致)
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
pub enum PosixSocketType {
    Stream = 1,
    Datagram = 2,
    Raw = 3,
    Rdm = 4,
    SeqPacket = 5,
    Dccp = 6,
    Packet = 10,
}

impl TryFrom<u8> for PosixSocketType {
    type Error = SystemError;
    fn try_from(x: u8) -> Result<Self, Self::Error> {
        use num_traits::FromPrimitive;
        return <Self as FromPrimitive>::from_u8(x).ok_or_else(|| SystemError::EINVAL);
    }
}

/// @brief Socket在文件系统中的inode封装
#[derive(Debug)]
pub struct SocketInode(SpinLock<Box<dyn Socket>>);

impl SocketInode {
    pub fn new(socket: Box<dyn Socket>) -> Arc<Self> {
        return Arc::new(Self(SpinLock::new(socket)));
    }

    #[inline]
    pub fn inner(&self) -> SpinLockGuard<Box<dyn Socket>> {
        return self.0.lock();
    }

    pub unsafe fn inner_no_preempt(&self) -> SpinLockGuard<Box<dyn Socket>> {
        return self.0.lock_no_preempt();
    }
}

impl IndexNode for SocketInode {
    fn open(
        &self,
        _data: &mut crate::filesystem::vfs::FilePrivateData,
        _mode: &crate::filesystem::vfs::file::FileMode,
    ) -> Result<(), SystemError> {
        return Ok(());
    }

    fn close(
        &self,
        _data: &mut crate::filesystem::vfs::FilePrivateData,
    ) -> Result<(), SystemError> {
        let mut socket = self.0.lock_irqsave();
        if let Some(Endpoint::Ip(Some(ip))) = socket.endpoint() {
            PORT_MANAGER.unbind_port(socket.metadata().unwrap().socket_type, ip.port)?;
        }

        let _ = socket.clear_epoll();

        HANDLE_MAP
            .write_irqsave()
            .remove(&socket.socket_handle())
            .unwrap();
        return Ok(());
    }

    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, SystemError> {
        return self.0.lock_no_preempt().read(&mut buf[0..len]).0;
    }

    fn write_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &[u8],
        _data: &mut crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, SystemError> {
        return self.0.lock_no_preempt().write(&buf[0..len], None);
    }

    fn poll(&self, _private_data: &FilePrivateData) -> Result<usize, SystemError> {
        let events = self.0.lock_irqsave().poll();
        return Ok(events.bits() as usize);
    }

    fn fs(&self) -> alloc::sync::Arc<dyn crate::filesystem::vfs::FileSystem> {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<Vec<alloc::string::String>, SystemError> {
        return Err(SystemError::ENOTDIR);
    }

    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        let meta = Metadata {
            mode: ModeType::from_bits_truncate(0o755),
            file_type: FileType::Socket,
            ..Default::default()
        };

        return Ok(meta);
    }

    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        return Ok(());
    }
}

/// ### 为socket提供无锁的poll方法
///
/// 因为在网卡中断中，需要轮询socket的状态，如果使用socket文件或者其inode来poll
/// 在当前的设计，会必然死锁，所以引用这一个设计来解决，提供无🔓的poll
pub struct SocketPollMethod;

impl SocketPollMethod {
    pub fn poll(socket: &socket::Socket, shutdown: ShutdownType) -> EPollEventType {
        match socket {
            socket::Socket::Raw(_) => todo!(),
            socket::Socket::Icmp(_) => todo!(),
            socket::Socket::Udp(udp) => Self::udp_poll(udp, shutdown),
            socket::Socket::Tcp(tcp) => Self::tcp_poll(tcp, shutdown),
            socket::Socket::Dhcpv4(_) => todo!(),
            socket::Socket::Dns(_) => todo!(),
        }
    }

    pub fn tcp_poll(socket: &socket::tcp::Socket, shutdown: ShutdownType) -> EPollEventType {
        let mut events = EPollEventType::empty();
        if socket.is_listening() && socket.is_active() {
            events.insert(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
            return events;
        }

        // socket已经关闭
        if !socket.is_open() {
            events.insert(EPollEventType::EPOLLHUP)
        }
        if shutdown.contains(ShutdownType::RCV_SHUTDOWN) {
            events.insert(
                EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM | EPollEventType::EPOLLRDHUP,
            );
        }

        let state = socket.state();
        if state != State::SynSent && state != State::SynReceived {
            // socket有可读数据
            if socket.can_recv() {
                events.insert(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
            }

            if !(shutdown.contains(ShutdownType::SEND_SHUTDOWN)) {
                // 缓冲区可写
                if socket.send_queue() < socket.send_capacity() {
                    events.insert(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM);
                } else {
                    // TODO：触发缓冲区已满的信号
                    todo!("A signal that the buffer is full needs to be sent");
                }
            } else {
                // 如果我们的socket关闭了SEND_SHUTDOWN，epoll事件就是EPOLLOUT
                events.insert(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM);
            }
        } else if state == State::SynSent {
            events.insert(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM);
        }

        // socket发生错误
        if !socket.is_active() {
            events.insert(EPollEventType::EPOLLERR);
        }

        events
    }

    pub fn udp_poll(socket: &socket::udp::Socket, shutdown: ShutdownType) -> EPollEventType {
        let mut event = EPollEventType::empty();

        if shutdown.contains(ShutdownType::RCV_SHUTDOWN) {
            event.insert(
                EPollEventType::EPOLLRDHUP | EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
            );
        }
        if shutdown.contains(ShutdownType::SHUTDOWN_MASK) {
            event.insert(EPollEventType::EPOLLHUP);
        }

        if socket.can_recv() {
            event.insert(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
        }

        if socket.can_send() {
            event.insert(
                EPollEventType::EPOLLOUT
                    | EPollEventType::EPOLLWRNORM
                    | EPollEventType::EPOLLWRBAND,
            );
        } else {
            // TODO: 缓冲区空间不够，需要使用信号处理
            todo!()
        }

        return event;
    }
}
