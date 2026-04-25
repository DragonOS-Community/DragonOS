use crate::net::socket::{
    netlink::{
        kobject::NetlinkKobjectUeventSocket,
        route::NetlinkRouteSocket,
        table::{is_valid_protocol, StandardNetlinkProtocol},
    },
    Socket, PSOCK,
};
use alloc::sync::Arc;
use system_error::SystemError;

pub mod addr;
mod common;
mod kobject;
mod message;
mod receiver;
mod route;
pub mod table;

pub fn create_netlink_socket(
    socket_type: PSOCK,
    protocol: u32,
    is_nonblock: bool,
) -> Result<Arc<dyn Socket>, SystemError> {
    if !matches!(socket_type, super::PSOCK::Raw | super::PSOCK::Datagram) {
        log::warn!("unsupported socket type for Netlink");
        return Err(SystemError::ESOCKTNOSUPPORT);
    }

    let inode: Arc<dyn Socket> = match StandardNetlinkProtocol::try_from(protocol) {
        Ok(StandardNetlinkProtocol::ROUTE) => {
            NetlinkRouteSocket::new(is_nonblock, socket_type, protocol)
        }
        Ok(StandardNetlinkProtocol::KOBJECT_UEVENT) => {
            NetlinkKobjectUeventSocket::new(is_nonblock, socket_type, protocol)
        }
        Ok(_) => {
            log::warn!(
                "standard netlink families {} is not supported yet",
                protocol
            );
            return Err(SystemError::EPROTONOSUPPORT);
        }
        Err(_) => {
            if !is_valid_protocol(protocol) {
                log::error!("invalid netlink protocol: {}", protocol);
            } else {
                log::error!("user-provided netlink family is not supported");
            }
            return Err(SystemError::EPROTONOSUPPORT);
        }
    };

    Ok(inode)
}
