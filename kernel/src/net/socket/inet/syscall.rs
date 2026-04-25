use alloc::sync::Arc;
use smoltcp::{self, wire::IpProtocol};
use system_error::SystemError;

use crate::net::socket::{
    inet::{RawSocket, TcpSocket, UdpSocket},
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
                return Ok(UdpSocket::new(is_nonblock, version));
            }
            _ => {
                return Err(SystemError::EPROTONOSUPPORT);
            }
        },
        PSOCK::Stream => match protocol {
            IpProtocol::HopByHop | IpProtocol::Tcp => {
                // log::debug!("create tcp socket");
                return Ok(TcpSocket::new(is_nonblock, version));
            }
            _ => {
                return Err(SystemError::EPROTONOSUPPORT);
            }
        },
        PSOCK::Raw => {
            // Raw socket 支持任意协议号
            // IPPROTO_RAW (255) 用于发送自定义 IP 包
            return Ok(RawSocket::new(version, protocol, is_nonblock)?);
        }
        _ => {
            return Err(SystemError::EPROTONOSUPPORT);
        }
    }
}
