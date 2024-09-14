use alloc::sync::Arc;
use inet::TcpSocket;
use smoltcp;
use system_error::SystemError::{self, *};

use super::InetSocket;

// use crate::net::syscall_util::SysArgSocketType;
use crate::net::socket::*;

fn create_inet_socket(
    socket_type: Type,
    protocol: smoltcp::wire::IpProtocol,
) -> Result<Arc<dyn Socket>, SystemError> {
    log::debug!("type: {:?}, protocol: {:?}", socket_type, protocol);
    use smoltcp::wire::IpProtocol::*;
    use Type::*;
    match socket_type {
        Datagram => {
            if !matches!(protocol, Udp) {
                return Err(EPROTONOSUPPORT);
            }
            todo!("udp")
        },
        Stream => {
            match protocol {
                HopByHop | Tcp => {
                    return Ok(TcpSocket::new(false));
                },
                _ => {
                    return Err(EPROTONOSUPPORT);
                }
            }
        },
        Raw => {
            todo!("raw")
        },
        _ => {
            return Err(EPROTONOSUPPORT);
        }
    }
}

pub struct Inet;
impl family::Family for Inet {
    fn socket(stype: Type, protocol: u32) -> Result<Arc<Inode>, SystemError> {
        let socket = create_inet_socket(stype, smoltcp::wire::IpProtocol::from(protocol as u8))?;
        Ok(Inode::new(socket))
    }
}
