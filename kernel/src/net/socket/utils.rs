use alloc::sync::Arc;
use crate::net::socket;
use socket::Family;
use system_error::SystemError;

pub fn create_socket(family: socket::AddressFamily, socket_type: socket::Type, protocol: u32, is_nonblock: bool, is_close_on_exec: bool) 
    -> Result<Arc<socket::Inode>, SystemError> {
    type AF = socket::AddressFamily;
    match family {
        AF::INet => {
            let inode = socket::inet::Inet::socket(socket_type, protocol);
            inode.set_nonblock(is_nonblock);
            inode.set_close_on_exec(is_close_on_exec);
            return Ok(inode);
        }
        AF::INet6 => {
            todo!("AF_INET6 unimplemented");
        }
        AF::Unix => {
            todo!("AF_UNIX unimplemented");
        }
        AF::Netlink => {
            todo!("AF_NETLINK unimplemented");
        }
        _ => {
            todo!("unsupport address family");
        }
    }
}