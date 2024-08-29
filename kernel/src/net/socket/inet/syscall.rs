use system_error::SystemError::{self, *};
use smoltcp;
use alloc::sync::Arc;

use super::AnyInetSocket;

// use crate::net::syscall_util::SysArgSocketType;
use crate::net::socket;


fn create_inet_socket(sock_type: socket::Type, protocol: smoltcp::wire::IpProtocol) -> Result<Arc<dyn AnyInetSocket>, SystemError> {
    use smoltcp::wire::IpProtocol::*;
    match protocol {
        Udp => {
            if !matches!(sock_type, socket::Type::Datagram) {
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
            if !matches!(sock_type, socket::Type::Raw) {
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
impl socket::family::Family for Inet {
    fn socket(stype: socket::Type, protocol: u32) -> Arc<socket::Inode> {
        // create_inet_socket(stype, protocol.into())
        todo!("{:?}{:?}", stype, protocol);
    }

    
}