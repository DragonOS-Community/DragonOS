use system_error::SystemError::{self, *};

pub mod raw;
pub mod datagram;
pub mod stream;
pub mod common;

pub use raw::RawSocket;
pub use datagram::UdpInner;
pub use stream::TcpSocket;

#[derive(Debug)]
pub enum InetSocket {
    Raw(RawSocket),
    Udp(UdpInner),
    Tcp(TcpSocket),
}

pub trait AnyInetSocket {
    fn bind(&self, endpoint: smoltcp::wire::IpEndpoint) -> Result<(), SystemError>;
    
    fn common(&self) -> &common::BoundInetInner;
    
    fn endpoint(&self) -> smoltcp::wire::IpEndpoint {
        self.common().endpoint()
    }

    fn remote(&self) -> Option<smoltcp::wire::IpEndpoint> {
        self.common().remote
    }

    fn connect(&self, endpoint: smoltcp::wire::IpEndpoint) -> Result<(), SystemError>;
    
    fn send(&self, data: &[u8]) -> Result<usize, SystemError>;
    
    fn recv(&self, data: &mut [u8]) -> Result<usize, SystemError>;
    
    fn close(&mut self);
}

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