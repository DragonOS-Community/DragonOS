use crate::net::socket::{
    netlink::{
        route::NetlinkRouteSocket,
        table::{is_valid_protocol, StandardNetlinkProtocol},
    },
    Socket, PSOCK,
};
use alloc::sync::Arc;
use system_error::SystemError;

pub mod addr;
mod common;
mod message;
mod receiver;
mod route;
pub mod table;

pub fn create_netlink_socket(
    socket_type: PSOCK,
    protocol: u32,
    _is_nonblock: bool,
) -> Result<Arc<dyn Socket>, SystemError> {
    match socket_type {
        super::PSOCK::Raw | super::PSOCK::Datagram => {
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

            Ok(inode)
        }
        _ => {
            log::warn!("unsupported socket type for Netlink");
            Err(SystemError::EPROTONOSUPPORT)
        }
    }
}
