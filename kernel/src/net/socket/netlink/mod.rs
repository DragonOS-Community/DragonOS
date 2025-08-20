use crate::net::socket::{
    family,
    netlink::{
        route::NetlinkRouteSocket,
        table::{is_valid_protocol, StandardNetlinkProtocol},
    },
    SocketInode,
};
use alloc::sync::Arc;
use system_error::SystemError;

pub mod addr;
mod common;
mod message;
mod receiver;
mod route;
pub mod table;

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
        Ok(_) => {
            log::warn!(
                "standard netlink families {} is not supported yet",
                protocol
            );
            return Err(SystemError::EAFNOSUPPORT);
        }
        Err(_) => {
            if is_valid_protocol(protocol) {
                log::error!("user-provided netlink family is not supported");
                return Err(SystemError::EPROTONOSUPPORT);
            }
            log::error!("invalid netlink protocol: {}", protocol);
            return Err(SystemError::EAFNOSUPPORT);
        }
    };

    Ok(SocketInode::new(inode))
}
