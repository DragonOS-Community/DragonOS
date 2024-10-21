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
const UNSPECIFIED_LOCAL_ENDPOINT_V4: IpEndpoint =
    IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), 0);
const UNSPECIFIED_LOCAL_ENDPOINT_V6: IpEndpoint =
    IpEndpoint::new(IpAddress::Ipv6(Ipv6Address::UNSPECIFIED), 0);

pub trait InetSocket: Socket {
    /// `on_iface_events`
    /// 通知socket发生的事件
    fn on_iface_events(&self);
}
