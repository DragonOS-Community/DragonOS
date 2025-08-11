use crate::net::socket::{
    family,
    netlink::{route::NetlinkRouteSocket, table::StandardNetlinkProtocol},
    SocketInode,
};
use alloc::sync::Arc;
use system_error::SystemError;

pub mod addr;
mod common;
pub mod message;
mod receiver;
mod route;
mod table;

pub struct Netlink;

impl family::Family for Netlink {
    fn socket(
        stype: super::PSOCK,
        protocol: u32,
    ) -> Result<alloc::sync::Arc<super::SocketInode>, SystemError> {
        match stype {
            super::PSOCK::Raw | super::PSOCK::Datagram => create_netlink_socket(protocol),
            _ => {
                log::warn!("unsupported socket type for Netlink");
                Err(SystemError::EPROTONOSUPPORT)
            }
        }
    }
}

fn create_netlink_socket(protocol: u32) -> Result<Arc<SocketInode>, SystemError> {
    let nl_protocol = StandardNetlinkProtocol::try_from(protocol);
    let inode = match nl_protocol {
        Ok(StandardNetlinkProtocol::ROUTE) => NetlinkRouteSocket::new(false),
        _ => {
            log::warn!("unsupported Netlink protocol: {}", protocol);
            return Err(SystemError::EPROTONOSUPPORT);
        }
    };

    Ok(SocketInode::new(inode))
}
