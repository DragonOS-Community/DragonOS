use alloc::sync::Arc;
use crate::net::socket;
use socket::Family;

pub fn create_socket(family: socket::AddressFamily, stype: socket::Type, protocol: u32, is_nonblock: bool, is_close_on_exec: bool) 
    -> Arc<socket::Inode> {
    type AF = socket::AddressFamily;
    match family {
        AF::INet => {
            let inode = socket::inet::Inet::socket(stype, protocol);
            inode.set_nonblock(is_nonblock);
            inode.set_close_on_exec(is_close_on_exec);
            inode
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