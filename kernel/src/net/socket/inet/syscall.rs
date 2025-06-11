use alloc::sync::Arc;
use smoltcp::{self, wire::IpProtocol};
use system_error::SystemError;

use crate::{
    filesystem::vfs::IndexNode,
    net::socket::{
        family,
        inet::{TcpSocket, UdpSocket},
        Socket, PSOCK,
    },
};

fn create_inet_socket(
    version: smoltcp::wire::IpVersion,
    socket_type: PSOCK,
    protocol: smoltcp::wire::IpProtocol,
    is_nonblock: bool,
) -> Result<Arc<dyn IndexNode>, SystemError> {
    // log::debug!("type: {:?}, protocol: {:?}", socket_type, protocol);
    match socket_type {
        PSOCK::Datagram => match protocol {
            IpProtocol::HopByHop | IpProtocol::Udp => {
                return Ok(UdpSocket::new(is_nonblock));
            }
            _ => {
                return Err(SystemError::EPROTONOSUPPORT);
            }
        },
        PSOCK::Stream => match protocol {
            IpProtocol::HopByHop | IpProtocol::Tcp => {
                log::debug!("create tcp socket");
                return Ok(TcpSocket::new(is_nonblock, version));
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
    fn socket(
        stype: PSOCK,
        protocol: u32,
        is_nonblock: bool,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        create_inet_socket(
            smoltcp::wire::IpVersion::Ipv4,
            stype,
            smoltcp::wire::IpProtocol::from(protocol as u8),
            is_nonblock,
        )
    }
}

pub struct Inet6;
impl family::Family for Inet6 {
    fn socket(
        stype: PSOCK,
        protocol: u32,
        is_nonblock: bool,
    ) -> Result<Arc<dyn Socket>, SystemError> {
        create_inet_socket(
            smoltcp::wire::IpVersion::Ipv6,
            stype,
            smoltcp::wire::IpProtocol::from(protocol as u8),
            is_nonblock,
        )
    }
}
