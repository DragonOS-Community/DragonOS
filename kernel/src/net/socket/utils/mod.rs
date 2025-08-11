use crate::net::socket::{
    self, inet::syscall::create_inet_socket, unix::create_unix_socket, Socket,
};
pub(super) mod datagram_common;

use crate::net::socket;
use alloc::sync::Arc;
use system_error::SystemError;

pub fn create_socket(
    family: socket::AddressFamily,
    socket_type: socket::PSOCK,
    protocol: u32,
    is_nonblock: bool,
    _is_close_on_exec: bool,
) -> Result<Arc<dyn Socket>, SystemError> {
    // log::info!("Creating socket: {:?}, {:?}, {:?}", family, socket_type, protocol);
    type AF = socket::AddressFamily;
    let inode = match family {
        AF::INet => socket::inet::Inet::socket(socket_type, protocol)?,
        // AF::INet6 => socket::inet::Inet6::socket(socket_type, protocol)?,
        AF::Unix => socket::unix::Unix::socket(socket_type, protocol)?,
        AF::Netlink => socket::netlink::Netlink::socket(socket_type, protocol)?,
        _ => {
            log::warn!("unsupport address family");
            return Err(SystemError::EAFNOSUPPORT);
        }
    };
    // inode.set_close_on_exec(is_close_on_exec);
    return Ok(inode);
}
