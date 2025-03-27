use alloc::sync::Arc;
use smoltcp::{self, wire::IpProtocol};
use system_error::SystemError;

use crate::net::socket::{
    family,
    inet::{TcpSocket, UdpSocket},
    Socket, SocketInode, PSOCK,
};

fn create_inet_socket(
    version: smoltcp::wire::IpVersion,
    socket_type: PSOCK,
    protocol: smoltcp::wire::IpProtocol,
) -> Result<Arc<dyn Socket>, SystemError> {
    // log::debug!("type: {:?}, protocol: {:?}", socket_type, protocol);
    match socket_type {
        PSOCK::Datagram => match protocol {
            IpProtocol::HopByHop | IpProtocol::Udp => {
                return Ok(UdpSocket::new(false));
            }
            _ => {
                return Err(SystemError::EPROTONOSUPPORT);
            }
        },
        PSOCK::Stream => match protocol {
            IpProtocol::HopByHop | IpProtocol::Tcp => {
                log::debug!("create tcp socket");
                return Ok(TcpSocket::new(false, version));
            }
            _ => {
                return Err(SystemError::EPROTONOSUPPORT);
            }
        },
        PSOCK::Raw => {
            todo!("raw")
        }
        _ => {
            return Err(SystemError::EPROTONOSUPPORT);
        }
    }
}

pub struct Inet;
impl family::Family for Inet {
    fn socket(stype: PSOCK, protocol: u32) -> Result<Arc<SocketInode>, SystemError> {
        let socket = create_inet_socket(
            smoltcp::wire::IpVersion::Ipv4,
            stype,
            smoltcp::wire::IpProtocol::from(protocol as u8),
        )?;
        Ok(SocketInode::new(socket))
    }
}

pub struct Inet6;
impl family::Family for Inet6 {
    fn socket(stype: PSOCK, protocol: u32) -> Result<Arc<SocketInode>, SystemError> {
        let socket = create_inet_socket(
            smoltcp::wire::IpVersion::Ipv6,
            stype,
            smoltcp::wire::IpProtocol::from(protocol as u8),
        )?;
        Ok(SocketInode::new(socket))
    }
}
