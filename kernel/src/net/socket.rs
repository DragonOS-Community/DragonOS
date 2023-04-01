use alloc::{boxed::Box, sync::Arc, vec::Vec};
use smoltcp::{
    iface::{self, SocketHandle, SocketSet},
    socket::{raw, tcp::{self, State}, udp},
    wire::{IpAddress, IpEndpoint, IpProtocol, Ipv4Address, Ipv4Packet, Ipv6Address},
};

use crate::{
    arch::rand::rand,
    driver::{net::NetDriver, NET_DRIVERS},
    include::bindings::bindings::ida,
    kdebug, kerror, kwarn,
    libs::{rwlock::RwLock, spinlock::SpinLock},
    net::{Interface, NET_FACES},
    syscall::SystemError,
};

use super::{Endpoint, Protocol, Socket};

lazy_static! {
    /// 所有socket的集合
    /// TODO: 优化这里，自己实现SocketSet！！！现在这样的话，不管全局有多少个网卡，每个时间点都只会有1个进程能够访问socket
    pub static ref SOCKET_SET: SpinLock<SocketSet<'static >> = SpinLock::new(SocketSet::new(vec![]));
}

/// @brief socket的句柄管理组件。
/// 它在smoltcp的SocketHandle上封装了一层，增加更多的功能。
/// 比如，在socket被关闭时，自动释放socket的资源，通知系统的其他组件。
#[derive(Debug)]
pub struct GlobalSocketHandle(SocketHandle);

impl GlobalSocketHandle {
    pub fn new(handle: SocketHandle) -> Self {
        Self(handle)
    }
}

impl Clone for GlobalSocketHandle {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl Drop for GlobalSocketHandle {
    fn drop(&mut self) {
        let mut socket_set_guard = SOCKET_SET.lock();
        socket_set_guard.remove(self.0); // 删除的时候，会发送一条FINISH的信息？
        drop(socket_set_guard);
    }
}

/// @brief socket的类型
#[derive(Debug)]
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

#[derive(Debug)]
/// @brief 在trait Socket的metadata函数中返回该结构体供外部使用
pub struct SocketMetadata {
    /// socket的类型
    socket_type: SocketType,
    /// 发送缓冲区的大小
    send_buf_size: usize,
    /// 接收缓冲区的大小
    recv_buf_size: usize,
    /// 元数据的缓冲区的大小
    metadata_buf_size: usize,
    /// socket的选项
    options: SocketOptions,
}

/// @brief 表示原始的socket。原始套接字绕过传输层协议（如 TCP 或 UDP）并提供对网络层协议（如 IP）的直接访问。
///
/// ref: https://man7.org/linux/man-pages/man7/raw.7.html
#[derive(Debug, Clone)]
pub struct RawSocket {
    handle: GlobalSocketHandle,
    /// 用户发送的数据包是否包含了IP头.
    /// 如果是true，用户发送的数据包，必须包含IP头。（即用户要自行设置IP头+数据）
    /// 如果是false，用户发送的数据包，不包含IP头。（即用户只要设置数据）
    header_included: bool,
    /// socket的选项
    options: SocketOptions,
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
            IpProtocol::from(protocol),
            tx_buffer,
            rx_buffer,
        );

        // 把socket添加到socket集合中，并得到socket的句柄
        let handle: GlobalSocketHandle = GlobalSocketHandle::new(SOCKET_SET.lock().add(socket));

        return Self {
            handle,
            header_included: false,
            options,
        };
    }
}

impl Socket for RawSocket {
    fn read(&self, buf: &mut [u8]) -> Result<(usize, Endpoint), SystemError> {
        loop {
            // 如何优化这里？
            let mut socket_set_guard = SOCKET_SET.lock();
            let socket = socket_set_guard.get_mut::<raw::Socket>(self.handle.0);

            match socket.recv_slice(buf) {
                Ok(len) => {
                    let packet = Ipv4Packet::new_unchecked(buf);
                    return Ok((
                        len,
                        Endpoint::Ip(smoltcp::wire::IpEndpoint {
                            addr: IpAddress::Ipv4(packet.src_addr()),
                            port: 0,
                        }),
                    ));
                }
                Err(smoltcp::socket::raw::RecvError::Exhausted) => {
                    if !self.options.contains(SocketOptions::BLOCK) {
                        // 如果是非阻塞的socket，就返回错误
                        return Err(SystemError::EAGAIN);
                    }
                }
            }
            drop(socket);
            drop(socket_set_guard);
        }
    }

    fn write(&self, buf: &[u8], to: Option<super::Endpoint>) -> Result<usize, SystemError> {
        // 如果用户发送的数据包，包含IP头，则直接发送
        if self.header_included {
            let mut socket_set_guard = SOCKET_SET.lock();
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

            if let Some(Endpoint::Ip(endpoint)) = to {
                let mut socket_set_guard = SOCKET_SET.lock();
                let socket: &mut raw::Socket =
                    socket_set_guard.get_mut::<raw::Socket>(self.handle.0);

                // 暴力解决方案：只考虑0号网卡。 TODO：考虑多网卡的情况！！！
                let iface: Arc<Interface> = NET_FACES.read().get(&0).unwrap().clone();

                // 构造IP头
                let ipv4_src_addr: Option<smoltcp::wire::Ipv4Address> =
                    iface.inner_iface.ipv4_addr();
                if ipv4_src_addr.is_none() {
                    return Err(SystemError::ENETUNREACH);
                }
                let ipv4_src_addr = ipv4_src_addr.unwrap();

                if let IpAddress::Ipv4(ipv4_dst) = endpoint.addr {
                    let len = buf.len();

                    // 创建20字节的IPv4头部
                    let mut buffer: Vec<u8> = vec![0u8; len + 20];
                    let mut packet: Ipv4Packet<&mut Vec<u8>> =
                        Ipv4Packet::new_unchecked(&mut buffer);

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

                    drop(socket);
                    drop(socket_set_guard);

                    // poll?
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

    fn connect(&mut self, endpoint: super::Endpoint) -> Result<(), SystemError> {
        return Ok(());
    }

    fn metadata(&self) -> Result<SocketMetadata, SystemError> {
        todo!()
    }

    fn box_clone(&self) -> alloc::boxed::Box<dyn Socket> {
        return Box::new(self.clone());
    }
}

/// @brief 表示udp socket
///
/// https://man7.org/linux/man-pages/man7/udp.7.html
#[derive(Debug, Clone)]
pub struct UdpSocket {
    pub handle: GlobalSocketHandle,
    remote_endpoint: Option<Endpoint>, // 记录远程endpoint提供给connect()， 应该使用IP地址。
    options: SocketOptions,
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
        let handle: GlobalSocketHandle = GlobalSocketHandle::new(SOCKET_SET.lock().add(socket));

        return Self {
            handle,
            remote_endpoint: None,
            options,
        };
    }
}

impl Socket for UdpSocket {
    /// @brief 在read函数执行之前，请先bind到本地的指定端口
    fn read(&self, buf: &mut [u8]) -> Result<(usize, Endpoint), SystemError> {
        loop {
            kdebug!("Wait22 to Read");

            let mut socket_set_guard = SOCKET_SET.lock();
            let socket = socket_set_guard.get_mut::<udp::Socket>(self.handle.0);

            kdebug!("Wait to Read");

            if socket.can_recv() {
                kdebug!("Can Receive");
                if let Ok((size, endpoint)) = socket.recv_slice(buf) {
                    drop(socket);
                    drop(socket_set_guard);

                    return Ok((size, Endpoint::Ip(endpoint)));
                }
            } else {
                // 没有数据可以读取. 如果没有bind到指定端口，也会导致rx_buf为空
                return Err(SystemError::EAGAIN);
            }
        }
    }

    fn write(&self, buf: &[u8], to: Option<super::Endpoint>) -> Result<usize, SystemError> {
        let endpoint: &IpEndpoint = {
            if let Some(Endpoint::Ip(ref endpoint)) = to {
                endpoint
            } else if let Some(Endpoint::Ip(ref endpoint)) = self.remote_endpoint {
                endpoint
            } else {
                return Err(SystemError::ENOTCONN);
            }
        };

        let mut socket_set_guard = SOCKET_SET.lock();
        let socket = socket_set_guard.get_mut::<udp::Socket>(self.handle.0);

        if socket.endpoint().port == 0 {
            let temp_port = get_ephemeral_port();

            match endpoint.addr {
                // 远程remote endpoint使用什么协议，发送的时候使用的协议是一样的吧
                // 否则就用 self.endpoint().addr.unwrap()
                IpAddress::Ipv4(_) => {
                    socket
                        .bind(IpEndpoint::new(
                            smoltcp::wire::IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
                            temp_port,
                        ))
                        .unwrap();
                }
                IpAddress::Ipv6(_) => {
                    socket
                        .bind(IpEndpoint::new(
                            smoltcp::wire::IpAddress::Ipv6(Ipv6Address::UNSPECIFIED),
                            temp_port,
                        ))
                        .unwrap();
                }
            }
        }

        return if socket.can_send() {
            match socket.send_slice(&buf, *endpoint) {
                Ok(()) => {
                    // avoid deadlock
                    drop(socket);
                    drop(socket_set_guard);

                    Ok(buf.len())
                }
                Err(_) => Err(SystemError::ENOBUFS),
            }
        } else {
            Err(SystemError::ENOBUFS)
        };
    }

    fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        let mut sockets = SOCKET_SET.lock();
        let socket = sockets.get_mut::<udp::Socket>(self.handle.0);

        return if let Endpoint::Ip(ip) = endpoint {
            match socket.bind(ip) {
                Ok(()) => Ok(()),
                Err(_) => Err(SystemError::EINVAL),
            }
        } else {
            Err(SystemError::EINVAL)
        };
    }

    /// @brief
    fn connect(&mut self, endpoint: super::Endpoint) -> Result<(), SystemError> {
        return if let Endpoint::Ip(_) = endpoint {
            self.remote_endpoint = Some(endpoint);
            Ok(())
        } else {
            Err(SystemError::EINVAL)
        };
    }

    fn metadata(&self) -> Result<SocketMetadata, SystemError> {
        todo!()
    }

    fn box_clone(&self) -> alloc::boxed::Box<dyn Socket> {
        return Box::new(self.clone());
    }
}

/// @brief 表示 tcp socket
///
/// https://man7.org/linux/man-pages/man7/tcp.7.html
#[derive(Debug, Clone)]
pub struct TcpSocket {
    handle: GlobalSocketHandle,
    local_endpoint: Option<IpEndpoint>, // save local endpoint for bind()
    is_listening: bool,
    options: SocketOptions,
}

impl TcpSocket {
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
        let tx_buffer = tcp::SocketBuffer::new(vec![0; Self::DEFAULT_TX_BUF_SIZE]);
        let rx_buffer = tcp::SocketBuffer::new(vec![0; Self::DEFAULT_RX_BUF_SIZE]);
        let socket = tcp::Socket::new(tx_buffer, rx_buffer);

        // 把socket添加到socket集合中，并得到socket的句柄
        let handle: GlobalSocketHandle = GlobalSocketHandle::new(SOCKET_SET.lock().add(socket));

        return Self {
            handle,
            local_endpoint: None,
            is_listening: false,
            options,
        };
    }
}

impl Socket for TcpSocket {
    /// @breif
    fn read(&self, buf: &mut [u8]) -> Result<(usize, Endpoint), SystemError> {
        loop {
            let mut socket_set_guard = SOCKET_SET.lock();
            let socket = socket_set_guard.get_mut::<tcp::Socket>(self.handle.0);

            if socket.may_recv() {
                if let Ok(size) = socket.recv_slice(buf) {
                    if size > 0 {
                        let endpoint = if let Some(p) = socket.remote_endpoint() {
                            p
                        } else {
                            return Err(SystemError::ENOTCONN);
                        };

                        drop(socket);
                        drop(socket_set_guard);

                        return Ok((size, Endpoint::Ip(endpoint)));
                    }
                }
            } else {
                return Err(SystemError::EAGAIN);
            }
        }
    }

    fn write(&self, buf: &[u8], to: Option<super::Endpoint>) -> Result<usize, SystemError> {
        let mut socket_set_guard = SOCKET_SET.lock();
        let socket = socket_set_guard.get_mut::<tcp::Socket>(self.handle.0);

        if socket.is_open() {
            if socket.can_send() {
                match socket.send_slice(buf) {
                    Ok(size) => {
                        drop(socket);
                        drop(socket_set_guard);

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

    fn connect(&mut self, endpoint: super::Endpoint) -> Result<(), SystemError> {
        // let mut sockets = SOCKET_SET.lock();
        // let mut socket = sockets.get::<tcp::Socket>(self.handle.0);

        // if let Endpoint::Ip(ip) = endpoint {
        //     let temp_port = if ip.port == 0 {
        //         get_ephemeral_port()
        //     } else {
        //         ip.port
        //     };

        //     return match socket.connect(iface.context(), temp_port) {
        //         Ok(()) => {
        //             // avoid deadlock
        //             drop(socket);
        //             drop(sockets);

        //             // wait for connection result
        //             loop {
        //                 // poll_ifaces();

        //                 let mut sockets = SOCKET_SET.lock();
        //                 let socket = sockets.get::<tcp::Socket>(self.handle.0);
        //                 match socket.state() {
        //                     State::SynSent => {
        //                         // still connecting
        //                         drop(socket);
        //                         kdebug!("poll for connection wait");
        //                         // SOCKET_ACTIVITY.wait(sockets);
        //                     }
        //                     State::Established => {
        //                         break Ok(());
        //                     }
        //                     _ => {
        //                         break Err(SystemError::ECONNREFUSED);
        //                     }
        //                 }
        //             }
        //         }
        //         Err(_) => Err(SystemError::ENOBUFS),
        //     };
        // } else {
        //     return Err(SystemError::EINVAL);
        // }
        return Err(SystemError::EINVAL);
    }

    /// @brief tcp socket 监听 local_endpoint 端口
    fn listen(&mut self, _backlog: usize) -> Result<(), SystemError> {
        if self.is_listening {
            return Ok(());
        }

        let local_endpoint = self.local_endpoint.ok_or(SystemError::EINVAL)?;
        let mut sockets = SOCKET_SET.lock();
        let mut socket = sockets.get_mut::<tcp::Socket>(self.handle.0);

        if socket.is_listening() {
            return Ok(());
        }

        return match socket.listen(local_endpoint) {
            Ok(()) => {
                self.is_listening = true;
                Ok(())
            }
            Err(_) => Err(SystemError::EINVAL),
        };
    }

    fn metadata(&self) -> Result<SocketMetadata, SystemError> {
        todo!()
    }

    fn box_clone(&self) -> alloc::boxed::Box<dyn Socket> {
        return Box::new(self.clone());
    }
}

/// @breif 自动分配一个未被使用的PORT
pub fn get_ephemeral_port() -> u16 {
    // TODO selects non-conflict high port
    static mut EPHEMERAL_PORT: u16 = 0;
    unsafe {
        if EPHEMERAL_PORT == 0 {
            EPHEMERAL_PORT = (49152 + rand() % (65536 - 49152)) as u16;
        }
        if EPHEMERAL_PORT == 65535 {
            EPHEMERAL_PORT = 49152;
        } else {
            EPHEMERAL_PORT = EPHEMERAL_PORT + 1;
        }
        EPHEMERAL_PORT
    }
}
