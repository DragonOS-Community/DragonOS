use smoltcp;

// pub mod raw;
// pub mod icmp;
pub mod common;
pub mod datagram;
pub mod stream;
pub mod syscall;

pub use common::BoundInner;
pub use common::Types;
// pub use raw::RawSocket;
pub use datagram::UdpSocket;
pub use stream::TcpSocket;
pub use syscall::Inet;

use super::Socket;

use smoltcp::wire::*;
/// A local endpoint, which indicates that the local endpoint is unspecified.
///
/// According to the Linux man pages and the Linux implementation, `getsockname()` will _not_ fail
/// even if the socket is unbound. Instead, it will return an unspecified socket address. This
/// unspecified endpoint helps with that.
const UNSPECIFIED_LOCAL_ENDPOINT: IpEndpoint =
    IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), 0);

pub trait InetSocket: Socket {
    /// `on_iface_events`
    /// 通知socket发生的事件
    fn on_iface_events(&self);
}

// #[derive(Debug)]
// pub enum InetSocket {
//     // Raw(RawSocket),
//     Udp(UdpSocket),
//     Tcp(TcpSocket),
// }

// impl InetSocket {
//     /// # `on_iface_events`
//     /// 通知socket发生了事件
//     pub fn on_iface_events(&self) {
//         todo!()
//     }
// }

// impl IndexNode for InetSocket {

// }

// impl Socket for InetSocket {
//     fn epoll_items(&self) -> &super::common::poll_unit::EPollItems {
//         match self {
//             InetSocket::Udp(udp) => udp.epoll_items(),
//             InetSocket::Tcp(tcp) => tcp.epoll_items(),
//         }
//     }

//     fn bind(&self, endpoint: crate::net::Endpoint) -> Result<(), SystemError> {
//         if let crate::net::Endpoint::Ip(ip) = endpoint {
//             match self {
//                 InetSocket::Udp(udp) => {
//                     udp.do_bind(ip)?;
//                 },
//                 InetSocket::Tcp(tcp) => {
//                     tcp.do_bind(ip)?;
//                 },
//             }
//             return Ok(());
//         }
//         return Err(EINVAL);
//     }

//     fn wait_queue(&self) -> &super::common::poll_unit::WaitQueue {
//         todo!()
//     }

//     fn on_iface_events(&self) {
//         todo!()
//     }
// }
