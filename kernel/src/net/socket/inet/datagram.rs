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
        event_poll::EPollEventType, net_core::poll_ifaces, socket::tcp_def::TcpOptions, syscall::PosixSocketOption, Endpoint, Protocol, NET_DEVICES, SocketOptionsLevel
    },
};

use crate::net::socket::{
    handle::GlobalSocketHandle, Socket, SocketMetadata,
    SocketOptions, SocketPollMethod, HANDLE_MAP, PORT_MANAGER, ip_def::IpOptions,
};

use super::common::{get_iface_to_bind, BoundInetInner, SocketType};

pub type SmolUdpSocket = smoltcp::socket::udp::Socket<'static>;

pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
pub const DEFAULT_RX_BUF_SIZE: usize = 64 * 1024;
pub const DEFAULT_TX_BUF_SIZE: usize = 64 * 1024;

#[derive(Debug)]
pub struct UnboundUdp {
    socket: SmolUdpSocket,
}

impl UnboundUdp {
    pub fn new() -> Self {
        let rx_buffer = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; DEFAULT_METADATA_BUF_SIZE],
            vec![0; DEFAULT_RX_BUF_SIZE],
        );
        let tx_buffer = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; DEFAULT_METADATA_BUF_SIZE],
            vec![0; DEFAULT_TX_BUF_SIZE],
        );
        let socket = SmolUdpSocket::new(rx_buffer, tx_buffer);

        return Self { socket };
    }

    pub fn bind(self, local_endpoint: smoltcp::wire::IpEndpoint) -> Result<BoundUdp, SystemError> {
        Ok( BoundUdp {
            inner: BoundInetInner::bind(self.socket, SocketType::Udp, local_endpoint)?,
        })
    }

    pub fn close(&mut self) {
        self.socket.close();
    }
}

#[derive(Debug)]
pub struct BoundUdp {
    inner: BoundInetInner,
}

impl BoundUdp {
    fn with_mut_socket<F, T>(&mut self, f: F) -> T
    where
        F: FnMut(&mut SmolUdpSocket) -> T,
    {
        self.inner.with_mut(f)
    }

    #[inline]
    fn try_recv(&mut self, buf: &mut [u8]) -> Result<(usize, smoltcp::wire::IpEndpoint), SystemError> {
        self.with_mut_socket(|socket| {
            if socket.can_recv() {
                if let Ok((size, metadata)) = socket.recv_slice(buf) {
                    return Ok((size, metadata.endpoint));
                }
            }
            return Err(ENOTCONN);
        })
    }

    fn try_send(&mut self, buf: &[u8], to: Option<smoltcp::wire::IpEndpoint>) -> Result<usize, SystemError> {
        let remote = to.or(self.inner.remote).ok_or(ENOTCONN)?;

        let result = self.with_mut_socket(|socket| {
            if socket.can_send() && socket.send_slice(buf, remote).is_ok() {
                return Ok(buf.len());
            }
            return Err(ENOBUFS);
        });
        return result;
    }

    fn close(&mut self) {
        self.with_mut_socket(|socket|{
            socket.close();
        });
        self.inner.iface().port_manager().unbind_port(SocketType::Udp, self.inner.endpoint().port);
    }
}

// Udp Inner 负责其内部资源管理
#[derive(Debug)]
pub enum UdpInner {
    Unbound(UnboundUdp),
    Bound(BoundUdp),
}

// Udp Socket 负责提供状态切换接口、执行状态切换
#[derive(Debug)]
pub struct UdpSocket {
    inner: RwLock<Option<UdpInner>>,
    metadata: SocketMetadata,
    
}

impl UdpSocket {
    pub fn new(options: SocketOptions) -> Self {
        let metadata = SocketMetadata::new(
            // SocketType::Udp,
            DEFAULT_RX_BUF_SIZE,
            DEFAULT_TX_BUF_SIZE,
            DEFAULT_METADATA_BUF_SIZE,
            options,
        );
        return Self {
            inner: RwLock::new(None),
            metadata,
        };
    }
}

bitflags! {
    pub struct UdpSocketOptions: u32 {
        const ZERO = 0;        /* No UDP options */
        const UDP_CORK = 1;         /* Never send partially complete segments */
        const UDP_ENCAP = 100;      /* Set the socket to accept encapsulated packets */
        const UDP_NO_CHECK6_TX = 101; /* Disable sending checksum for UDP6X */
        const UDP_NO_CHECK6_RX = 102; /* Disable accepting checksum for UDP6 */
        const UDP_SEGMENT = 103;    /* Set GSO segmentation size */
        const UDP_GRO = 104;        /* This socket can receive UDP GRO packets */

        const UDPLITE_SEND_CSCOV = 10; /* sender partial coverage (as sent)      */
        const UDPLITE_RECV_CSCOV = 11; /* receiver partial coverage (threshold ) */
    }
}

bitflags! {
    pub struct UdpEncapTypes: u8 {
        const ZERO = 0;
        const ESPINUDP_NON_IKE = 1;     // draft-ietf-ipsec-nat-t-ike-00/01
        const ESPINUDP = 2;             // draft-ietf-ipsec-udp-encaps-06
        const L2TPINUDP = 3;            // rfc2661
        const GTP0 = 4;                 // GSM TS 09.60
        const GTP1U = 5;                // 3GPP TS 29.060
        const RXRPC = 6;
        const ESPINTCP = 7;             // Yikes, this is really xfrm encap types.
    }
}


// fn sock_set_option(
//     &self,
//     _socket: &mut udp::Socket,
//     _level: SocketOptionsLevel,
//     optname: PosixSocketOption,
//     _optval: &[u8],
// ) -> Result<(), SystemError> {
//     use PosixSocketOption::*;
//     use SystemError::*;

//     if optname == SO_BINDTODEVICE {
//         todo!("SO_BINDTODEVICE");
//     }

//     match optname {
//         SO_TYPE => {}
//         SO_PROTOCOL => {}
//         SO_DOMAIN => {}
//         SO_ERROR => {
//             return Err(ENOPROTOOPT);
//         }
//         SO_TIMESTAMP_OLD => {}
//         SO_TIMESTAMP_NEW => {}
//         SO_TIMESTAMPNS_OLD => {}
        
//         SO_TIMESTAMPING_OLD => {}
        
//         SO_RCVTIMEO_OLD => {}

//         SO_SNDTIMEO_OLD => {}
        
//         // if define CONFIG_NET_RX_BUSY_POLL
//         SO_BUSY_POLL | SO_PREFER_BUSY_POLL | SO_BUSY_POLL_BUDGET => {
//             debug!("Unsupported socket option: {:?}", optname);
//             return Err(ENOPROTOOPT);
//         }
//         // end if
//         optname => {
//             debug!("Unsupported socket option: {:?}", optname);
//             return Err(ENOPROTOOPT);
//         }
//     }
//     return Ok(());
// }

// fn udp_set_option(
//     &self,
//     level: SocketOptionsLevel,
//     optname: usize,
//     optval: &[u8],
// ) -> Result<(), SystemError> {
//     use PosixSocketOption::*;

//     let so_opt_name = 
//         PosixSocketOption::try_from(optname as i32)
//             .map_err(|_| SystemError::ENOPROTOOPT)?;

//     if level == SocketOptionsLevel::SOL_SOCKET {
//         self.with_mut_socket(f)
//         self.sock_set_option(self., level, so_opt_name, optval)?;
//         if so_opt_name == SO_RCVBUF || so_opt_name == SO_RCVBUFFORCE {
//             todo!("SO_RCVBUF");
//         }
//     }

//     match UdpSocketOptions::from_bits_truncate(optname as u32) {
//         UdpSocketOptions::UDP_CORK => {
//             todo!("UDP_CORK");
//         }
//         UdpSocketOptions::UDP_ENCAP => {
//             match UdpEncapTypes::from_bits_truncate(optval[0]) {
//                 UdpEncapTypes::ESPINUDP_NON_IKE => {
//                     todo!("ESPINUDP_NON_IKE");
//                 }
//                 UdpEncapTypes::ESPINUDP => {
//                     todo!("ESPINUDP");
//                 }
//                 UdpEncapTypes::L2TPINUDP => {
//                     todo!("L2TPINUDP");
//                 }
//                 UdpEncapTypes::GTP0 => {
//                     todo!("GTP0");
//                 }
//                 UdpEncapTypes::GTP1U => {
//                     todo!("GTP1U");
//                 }
//                 UdpEncapTypes::RXRPC => {
//                     todo!("RXRPC");
//                 }
//                 UdpEncapTypes::ESPINTCP => {
//                     todo!("ESPINTCP");
//                 }
//                 UdpEncapTypes::ZERO => {}
//                 _ => {
//                     return Err(SystemError::ENOPROTOOPT);
//                 }
//             }
//         }
//         UdpSocketOptions::UDP_NO_CHECK6_TX => {
//             todo!("UDP_NO_CHECK6_TX");
//         }
//         UdpSocketOptions::UDP_NO_CHECK6_RX => {
//             todo!("UDP_NO_CHECK6_RX");
//         }
//         UdpSocketOptions::UDP_SEGMENT => {
//             todo!("UDP_SEGMENT");
//         }
//         UdpSocketOptions::UDP_GRO => {
//             todo!("UDP_GRO");
//         }

//         UdpSocketOptions::UDPLITE_RECV_CSCOV => {
//             todo!("UDPLITE_RECV_CSCOV");
//         }
//         UdpSocketOptions::UDPLITE_SEND_CSCOV => {
//             todo!("UDPLITE_SEND_CSCOV");
//         }

//         UdpSocketOptions::ZERO => {}
//         _ => {
//             return Err(SystemError::ENOPROTOOPT);
//         }
//     }
//     return Ok(());
// }
