use crate::net::socket;
use alloc::sync::Arc;
use socket::Family;
use system_error::SystemError;

pub fn create_socket(
    family: socket::AddressFamily,
    socket_type: socket::Type,
    protocol: u32,
    is_nonblock: bool,
    is_close_on_exec: bool,
) -> Result<Arc<socket::Inode>, SystemError> {
    type AF = socket::AddressFamily;
    let inode = match family {
        AF::INet => socket::inet::Inet::socket(socket_type, protocol)?,
        AF::INet6 => {
            todo!("AF_INET6 unimplemented");
        }
        AF::Unix => socket::unix::Unix::socket(socket_type, protocol)?,
        AF::Netlink => socket::netlink::Netlink::socket(socket_type, protocol)?,
        _ => {
            todo!("unsupport address family");
        }
    };
    // inode.set_nonblock(is_nonblock);
    // inode.set_close_on_exec(is_close_on_exec);
    return Ok(inode);
}
