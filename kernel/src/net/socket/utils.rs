use crate::net::socket::{self, inet::syscall::create_inet_socket, Socket};
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
        AF::INet => create_inet_socket(
            smoltcp::wire::IpVersion::Ipv4,
            socket_type,
            smoltcp::wire::IpProtocol::from(protocol as u8),
            is_nonblock,
        )?,
        AF::INet6 => create_inet_socket(
            smoltcp::wire::IpVersion::Ipv6,
            socket_type,
            smoltcp::wire::IpProtocol::from(protocol as u8),
            is_nonblock,
        )?,
        // AF::Unix => socket::unix::Unix::socket(socket_type, protocol, is_nonblock)?,
        _ => {
            log::warn!("unsupport address family");
            return Err(SystemError::EAFNOSUPPORT);
        }
    };
    // inode.set_close_on_exec(is_close_on_exec);
    return Ok(inode);
}
