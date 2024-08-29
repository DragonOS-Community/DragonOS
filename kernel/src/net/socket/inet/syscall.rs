use system_error::SystemError::{self, *};
use smoltcp;
use alloc::sync::Arc;

use super::AnyInetSocket;

pub fn create_inet_socket(sock_type: crate::net::socket::define::SysArgSocketType, protocol: smoltcp::wire::IpProtocol) -> Result<Arc<dyn AnyInetSocket>, SystemError> {
    use crate::net::socket::define::SysArgSocketType as SocketTypes;
    use smoltcp::wire::IpProtocol::*;
    match protocol {
        Udp => {
            if sock_type.types() != SocketTypes::DGRAM {
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
            if sock_type.types() != SocketTypes::RAW {
                return Err(EPROTONOSUPPORT);
            }
            todo!()
        }
        _ => {
            return Err(EPROTONOSUPPORT);
        }
    }
}

use crate::net::socket;

pub struct Inet;
impl socket::family::Family for Inet {
    fn socket(stype: socket::Type, protocol: u32) -> Arc<socket::Inode> {
        todo!()
    }

    
}