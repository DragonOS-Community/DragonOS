use system_error::SystemError::{self, *};
use smoltcp;
use alloc::sync::Arc;

// pub mod raw;
// pub mod icmp;
pub mod datagram;
pub mod stream;
pub mod common;
pub mod syscall;

pub use common::Types;
pub use common::BoundInner;
// pub use raw::RawSocket;
pub use datagram::UdpSocket;
pub use stream::TcpSocket;

use crate::filesystem::vfs::IndexNode;

use super::Socket;

pub trait AnyInetSocket {
    fn epoll_items(&self) -> &super::common::poll_unit::EPollItems;
    fn wait_queue(&self) -> &super::common::poll_unit::WaitQueue;
    /// `on_iface_events` 
    /// 通知socket发生的事件
    fn on_iface_events(&self);
    fn do_bind(&self, endpoint: smoltcp::wire::IpEndpoint) -> Result<(), SystemError>;
}

#[derive(Debug)]
pub enum InetSocket {
    // Raw(RawSocket),
    Udp(UdpSocket),
    Tcp(TcpSocket),
}

impl InetSocket {
    pub fn on_iface_events(&self) {
        todo!()
    }
}

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

// pub trait Socket: FileLike + Send + Sync {
//     /// Assign the address specified by socket_addr to the socket
//     fn bind(&self, _socket_addr: SocketAddr) -> Result<()> {
//         return_errno_with_message!(Errno::EOPNOTSUPP, "bind() is not supported");
//     }

//     /// Build connection for a given address
//     fn connect(&self, _socket_addr: SocketAddr) -> Result<()> {
//         return_errno_with_message!(Errno::EOPNOTSUPP, "connect() is not supported");
//     }

//     /// Listen for connections on a socket
//     fn listen(&self, _backlog: usize) -> Result<()> {
//         return_errno_with_message!(Errno::EOPNOTSUPP, "listen() is not supported");
//     }

//     /// Accept a connection on a socket
//     fn accept(&self) -> Result<(Arc<dyn FileLike>, SocketAddr)> {
//         return_errno_with_message!(Errno::EOPNOTSUPP, "accept() is not supported");
//     }

//     /// Shut down part of a full-duplex connection
//     fn shutdown(&self, _cmd: SockShutdownCmd) -> Result<()> {
//         return_errno_with_message!(Errno::EOPNOTSUPP, "shutdown() is not supported");
//     }

//     /// Get address of this socket.
//     fn addr(&self) -> Result<SocketAddr> {
//         return_errno_with_message!(Errno::EOPNOTSUPP, "getsockname() is not supported");
//     }

//     /// Get address of peer socket
//     fn peer_addr(&self) -> Result<SocketAddr> {
//         return_errno_with_message!(Errno::EOPNOTSUPP, "getpeername() is not supported");
//     }

//     /// Get options on the socket. The resulted option will put in the `option` parameter, if
//     /// this method returns success.
//     fn get_option(&self, _option: &mut dyn SocketOption) -> Result<()> {
//         return_errno_with_message!(Errno::EOPNOTSUPP, "getsockopt() is not supported");
//     }

//     /// Set options on the socket.
//     fn set_option(&self, _option: &dyn SocketOption) -> Result<()> {
//         return_errno_with_message!(Errno::EOPNOTSUPP, "setsockopt() is not supported");
//     }

//     /// Sends a message on a socket.
//     fn sendmsg(
//         &self,
//         io_vecs: &[IoVec],
//         message_header: MessageHeader,
//         flags: SendRecvFlags,
//     ) -> Result<usize>;

//     /// Receives a message from a socket.
//     ///
//     /// If successful, the `io_vecs` buffer will be filled with the received content.
//     /// This method returns the length of the received message,
//     /// and the message header.
//     fn recvmsg(&self, io_vecs: &[IoVec], flags: SendRecvFlags) -> Result<(usize, MessageHeader)>;
// }