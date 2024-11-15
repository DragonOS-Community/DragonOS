use alloc::sync::Arc;
use smoltcp;
use system_error::SystemError::{self, *};

use inet::{TcpSocket, UdpSocket};

// use crate::net::syscall_util::SysArgSocketType;
use crate::net::socket::*;

fn create_inet_socket(
    version: smoltcp::wire::IpVersion,
    socket_type: PSOCK,
    protocol: smoltcp::wire::IpProtocol,
) -> Result<Arc<dyn Socket>, SystemError> {
    log::debug!("type: {:?}, protocol: {:?}", socket_type, protocol);
    use smoltcp::wire::IpProtocol::*;
    match socket_type {
        PSOCK::Datagram => match protocol {
            HopByHop | Udp => {
                log::debug!("create udp socket");
                // return Err(EPROTONOSUPPORT);
                return Ok(UdpSocket::new(false));
            }
            _ => {
                return Err(EPROTONOSUPPORT);
            }
        },
        PSOCK::Stream => match protocol {
            HopByHop | Tcp => {
                log::debug!("create tcp socket");
                return Ok(TcpSocket::new(false, version));
            }
            _ => {
                return Err(EPROTONOSUPPORT);
            }
        },
        PSOCK::Raw => {
            todo!("raw")
        }
        _ => {
            return Err(EPROTONOSUPPORT);
        }
    }
}

pub struct Inet;
impl family::Family for Inet {
    fn socket(stype: PSOCK, protocol: u32) -> Result<Arc<Inode>, SystemError> {
        let socket = create_inet_socket(
            smoltcp::wire::IpVersion::Ipv4,
            stype,
            smoltcp::wire::IpProtocol::from(protocol as u8),
        )?;
        Ok(Inode::new(socket))
    }
}

pub struct Inet6;
impl family::Family for Inet6 {
    fn socket(stype: PSOCK, protocol: u32) -> Result<Arc<Inode>, SystemError> {
        let socket = create_inet_socket(
            smoltcp::wire::IpVersion::Ipv6,
            stype,
            smoltcp::wire::IpProtocol::from(protocol as u8),
        )?;
        Ok(Inode::new(socket))
    }
}
