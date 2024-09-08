use system_error::SystemError::{self, *};
use smoltcp;
use alloc::sync::Arc;

use super::InetSocket;

// use crate::net::syscall_util::SysArgSocketType;
use crate::net::socket::*;


fn create_inet_socket(sock_type: Type, protocol: smoltcp::wire::IpProtocol) -> Result<Arc<dyn Socket>, SystemError> {
    use smoltcp::wire::IpProtocol::*;
    match protocol {
        Udp => {
            if !matches!(sock_type, Type::Datagram) {
                return Err(EPROTONOSUPPORT);
            }
            todo!()
        }
        Tcp => {
            todo!()
        }
        Icmp => {
            todo!()
        }
        HopByHop => {
            if !matches!(sock_type, Type::Raw) {
                return Err(EPROTONOSUPPORT);
            }
            todo!()
        }
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