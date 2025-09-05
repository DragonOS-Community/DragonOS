use alloc::sync::Arc;
use smoltcp::{self, wire::IpProtocol};
use system_error::SystemError;

use crate::net::socket::{
    inet::{TcpSocket, UdpSocket},
    Socket, PSOCK,
};

pub fn create_inet_socket(
    version: smoltcp::wire::IpVersion,
    socket_type: PSOCK,
    protocol: smoltcp::wire::IpProtocol,
    is_nonblock: bool,
) -> Result<Arc<dyn Socket>, SystemError> {
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
