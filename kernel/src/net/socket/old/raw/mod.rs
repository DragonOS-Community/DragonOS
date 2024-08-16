use alloc::{boxed::Box, sync::Arc, vec::Vec};
use log::{debug, error, warn};
use smoltcp::{
    socket::{raw, tcp, udp},
    wire,
};
use system_error::SystemError::{self, *};

use crate::{
    driver::net::Iface,
    libs::rwlock::RwLock,
    net::{
        event_poll::EPollEventType, net_core::poll_ifaces, syscall::PosixSocketOption, Endpoint, Protocol, NET_DEVICES, SocketOptionsLevel
    },
};

use crate::net::socket::{
    handle::GlobalSocketHandle, Socket, SocketMetadata,
    Options, SocketPollMethod, ip_def::IpOptions,
};

use super::common::{get_iface_to_bind, BoundInner, Types};

pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
pub const DEFAULT_RX_BUF_SIZE: usize = 64 * 1024;
pub const DEFAULT_TX_BUF_SIZE: usize = 64 * 1024;

pub const ICMP_FILTER: usize = 1;

pub type SmolRawSocket = smoltcp::socket::raw::Socket<'static>;

// #[derive(Debug)]
// pub struct NewRaw {
//     version: Option<smoltcp::wire::IpVersion>,
//     protocol: smoltcp::wire::IpProtocol,
// }

// impl NewRaw {
//     pub fn new(protocol: smoltcp::wire::IpProtocol) -> Self {
//         return Self {
//             version: None,
//             protocol,
//         };
//     }
// }

// #[derive(Debug)]
// pub struct UnboundRaw {
//     socket: SmolRawSocket,
// }

// impl UnboundRaw {
//     pub fn new(protocol: smoltcp::wire::IpProtocol) -> Self {
//         let rx_buffer = smoltcp::socket::raw::PacketBuffer::new(
//             vec![smoltcp::socket::raw::PacketMetadata::EMPTY; DEFAULT_METADATA_BUF_SIZE],
//             vec![0; DEFAULT_RX_BUF_SIZE],
//         );

//         let tx_buffer = smoltcp::socket::raw::PacketBuffer::new(
//             vec![smoltcp::socket::raw::PacketMetadata::EMPTY; DEFAULT_METADATA_BUF_SIZE],
//             vec![0; DEFAULT_TX_BUF_SIZE],
//         );

//         let socket = SmolRawSocket::new(
//             smoltcp::wire::IpVersion::Ipv4,
//             protocol,
//             rx_buffer, 
//             tx_buffer
//         );

//         return Self { socket };
//     }

//     pub fn bind(self, local_endpoint: smoltcp::wire::IpEndpoint) -> Result<BoundRaw, SystemError> {
//         Ok( BoundRaw {
//             inner: BoundInetInner::bind(self.socket, SocketType::Udp, local_endpoint)?,
//         })
//     }
// }

// #[derive(Debug)]
// pub struct BoundRaw {
//     inner: BoundInetInner,
// }

// impl BoundRaw {
//     fn with_mut_socket<F, T>(&mut self, f: F) -> T
//     where
//         F: FnMut(&mut SmolRawSocket) -> T,
//     {
//         self.inner.with_mut(f)
//     }

//     #[inline]
//     fn try_recv(&mut self, buf: &mut [u8]) -> Result<(usize, smoltcp::wire::IpEndpoint), SystemError> {
//         self.with_mut_socket(|socket| {
//             if socket.can_recv() {
//                 if let Ok((size, metadata)) = socket.recv_slice(buf) {
//                     return Ok((size, metadata.endpoint));
//                 }
//             }
//             return Err(ENOTCONN);
//         })
//     }

//     fn try_send(&mut self, buf: &[u8], to: Option<smoltcp::wire::IpEndpoint>) -> Result<usize, SystemError> {
//         let remote = to.or(self.inner.remote).ok_or(ENOTCONN)?;

//         let result = self.with_mut_socket(|socket| {
//             if socket.can_send() && socket.send_slice(buf, remote).is_ok() {
//                 return Ok(buf.len());
//             }
//             return Err(ENOBUFS);
//         });
//         return result;
//     }

//     fn close(&mut self) {
//         self.with_mut_socket(|socket|{
//             socket.close();
//         });
//         self.inner.iface().port_manager().unbind_port(SocketType::Udp, self.inner.endpoint().port);
//     }
// }

// // Udp Inner 负责其内部资源管理
// #[derive(Debug)]
// pub enum UdpInner {
//     Unbound(UnboundUdp),
//     Bound(BoundUdp),
// }

// // Udp Socket 负责提供状态切换接口、执行状态切换
// #[derive(Debug)]
// pub struct UdpSocket {
//     inner: RwLock<Option<UdpInner>>,
//     metadata: SocketMetadata,
    
// }

// impl UdpSocket {
//     pub fn new(options: SocketOptions) -> Self {
//         let metadata = SocketMetadata::new(
//             // SocketType::Udp,
//             DEFAULT_RX_BUF_SIZE,
//             DEFAULT_TX_BUF_SIZE,
//             DEFAULT_METADATA_BUF_SIZE,
//             options,
//         );
//         return Self {
//             inner: RwLock::new(None),
//             metadata,
//         };
//     }
// }

// /// @brief 表示原始的socket。原始套接字绕过传输层协议（如 TCP 或 UDP）并提供对网络层协议（如 IP）的直接访问。
// ///
// /// ref: https://man7.org/linux/man-pages/man7/raw.7.html
// #[derive(Debug, Clone)]
// pub struct RawSocket {
//     handle: GlobalSocketHandle,
//     /// 用户发送的数据包是否包含了IP头.
//     /// 如果是true，用户发送的数据包，必须包含IP头。（即用户要自行设置IP头+数据）
//     /// 如果是false，用户发送的数据包，不包含IP头。（即用户只要设置数据）
//     header_included: bool,
//     /// socket的metadata
//     metadata: SocketMetadata,
// }


// impl RawSocket {

//     /// @brief 创建一个原始的socket
//     ///
//     /// @param protocol 协议号
//     /// @param options socket的选项
//     ///
//     /// @return 返回创建的原始的socket
//     pub fn new(protocol: Protocol, options: SocketOptions) -> Self {
//         let rx_buffer = raw::PacketBuffer::new(
//             vec![raw::PacketMetadata::EMPTY; DEFAULT_METADATA_BUF_SIZE],
//             vec![0; DEFAULT_RX_BUF_SIZE],
//         );
//         let tx_buffer = raw::PacketBuffer::new(
//             vec![raw::PacketMetadata::EMPTY; DEFAULT_METADATA_BUF_SIZE],
//             vec![0; DEFAULT_TX_BUF_SIZE],
//         );
//         let protocol: u8 = protocol.into();
//         let socket = raw::Socket::new(
//             wire::IpVersion::Ipv4,
//             wire::IpProtocol::from(protocol),
//             rx_buffer,
//             tx_buffer,
//         );

//         // 把socket添加到socket集合中，并得到socket的句柄
//         let handle = GlobalSocketHandle::new_smoltcp_handle(SOCKET_SET.lock_irqsave().add(socket));

//         let metadata = SocketMetadata::new(
//             InetSocketType::Raw,
//             DEFAULT_RX_BUF_SIZE,
//             DEFAULT_TX_BUF_SIZE,
//             DEFAULT_METADATA_BUF_SIZE,
//             options,
//         );

//         let posix_item = Arc::new(PosixSocketHandleItem::new(None));

//         return Self {
//             handle,
//             header_included: false,
//             metadata,
//         };
//     }
// }

// impl Socket for RawSocket {

//     fn close(&mut self) {
//         let mut socket_set_guard = SOCKET_SET.lock_irqsave();
//         if let smoltcp::socket::Socket::Udp(mut sock) =
//             socket_set_guard.remove(self.handle.smoltcp_handle().unwrap())
//         {
//             sock.close();
//         }
//         drop(socket_set_guard);
//         poll_ifaces();
//     }

//     fn read(&self, buf: &mut [u8]) -> (Result<usize, SystemError>, Endpoint) {

//         let mut socket_set_guard = SOCKET_SET.lock_irqsave();
//         let socket =
//             socket_set_guard.get_mut::<raw::Socket>(self.handle.smoltcp_handle().unwrap());

//         match socket.recv_slice(buf) {
//             Ok(len) => {
//                 let packet = wire::Ipv4Packet::new_unchecked(buf);
//                 return (
//                     Ok(len),
//                     Endpoint::Ip(Some(wire::IpEndpoint {
//                         addr: wire::IpAddress::Ipv4(packet.src_addr()),
//                         port: 0,
//                     })),
//                 );
//             }
//             Err(_) => {
//                 return (Err(SystemError::EAGAIN_OR_EWOULDBLOCK), Endpoint::Ip(None))
//             }
//         }
//     }

//     fn write(&self, buf: &[u8], to: Option<Endpoint>) -> Result<usize, SystemError> {
//         // 如果用户发送的数据包，包含IP头，则直接发送
//         if self.header_included {
//             let mut socket_set_guard = SOCKET_SET.lock_irqsave();
//             let socket =
//                 socket_set_guard.get_mut::<raw::Socket>(self.handle.smoltcp_handle().unwrap());
//             match socket.send_slice(buf) {
//                 Ok(_) => {
//                     return Ok(buf.len());
//                 }
//                 Err(raw::SendError::BufferFull) => {
//                     return Err(SystemError::ENOBUFS);
//                 }
//             }
//         } else {
//             // 如果用户发送的数据包，不包含IP头，则需要自己构造IP头

//             if let Some(Endpoint::Ip(Some(endpoint))) = to {
//                 let mut socket_set_guard = SOCKET_SET.lock_irqsave();
//                 let socket: &mut raw::Socket =
//                     socket_set_guard.get_mut::<raw::Socket>(self.handle.smoltcp_handle().unwrap());

//                 // 暴力解决方案：只考虑0号网卡。 TODO：考虑多网卡的情况！！！
//                 let iface = NET_DEVICES.read_irqsave().get(&0).unwrap().clone();

//                 // 构造IP头
//                 let ipv4_src_addr: Option<wire::Ipv4Address> =
//                     iface.inner_iface().lock().ipv4_addr();
//                 if ipv4_src_addr.is_none() {
//                     return Err(SystemError::ENETUNREACH);
//                 }
//                 let ipv4_src_addr = ipv4_src_addr.unwrap();

//                 if let wire::IpAddress::Ipv4(ipv4_dst) = endpoint.addr {
//                     let len = buf.len();

//                     // 创建20字节的IPv4头部
//                     let mut buffer: Vec<u8> = vec![0u8; len + 20];
//                     let mut packet: wire::Ipv4Packet<&mut Vec<u8>> =
//                         wire::Ipv4Packet::new_unchecked(&mut buffer);

//                     // 封装ipv4 header
//                     packet.set_version(4);
//                     packet.set_header_len(20);
//                     packet.set_total_len((20 + len) as u16);
//                     packet.set_src_addr(ipv4_src_addr);
//                     packet.set_dst_addr(ipv4_dst);

//                     // 设置ipv4 header的protocol字段
//                     packet.set_next_header(socket.ip_protocol());

//                     // 获取IP数据包的负载字段
//                     let payload: &mut [u8] = packet.payload_mut();
//                     payload.copy_from_slice(buf);

//                     // 填充checksum字段
//                     packet.fill_checksum();

//                     // 发送数据包
//                     socket.send_slice(&buffer).unwrap();

//                     iface.poll().ok();

//                     drop(socket_set_guard);
//                     return Ok(len);
//                 } else {
//                     warn!("Unsupport Ip protocol type!");
//                     return Err(SystemError::EINVAL);
//                 }
//             } else {
//                 // 如果没有指定目的地址，则返回错误
//                 return Err(SystemError::ENOTCONN);
//             }
//         }
//     }

//     fn connect(&mut self, _endpoint: Endpoint) -> Result<(), SystemError> {
//         Ok(())
//     }

//     fn metadata(&self) -> SocketMetadata {
//         self.metadata.clone()
//     }

//     fn box_clone(&self) -> Box<dyn Socket> {
//         Box::new(self.clone())
//     }

//     /// @brief 设置socket的选项
//     ///
//     /// @param level 选项的层次
//     /// @param optname 选项的名称
//     /// @param optval 选项的值
//     ///
//     /// @return 返回设置是否成功, 如果不支持该选项，返回ENOSYS
//     /// 
//     /// ## See
//     /// https://code.dragonos.org.cn/s?refs=sk_setsockopt&project=linux-6.6.21
//     fn set_option(
//         &self,
//         _level: SocketOptionsLevel,
//         optname: usize,
//         _optval: &[u8],
//     ) -> Result<(), SystemError> {
//         if optname == ICMP_FILTER {
//             todo!("setsockopt ICMP_FILTER");
//         }
//         return Err(SystemError::ENOPROTOOPT);
//     }

//     fn socket_handle(&self) -> GlobalSocketHandle {
//         self.handle
//     }

//     fn as_any_ref(&self) -> &dyn core::any::Any {
//         self
//     }

//     fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
//         self
//     }
// }