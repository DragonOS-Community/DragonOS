use crate::net::syscall::SockAddrNl;
#[derive(Debug)]
#[derive(Clone)]
pub struct NetlinkEndpoint{
    pub addr: SockAddrNl,
    pub addr_len: usize
}
impl NetlinkEndpoint {
    pub fn new(addr: SockAddrNl, addr_len: usize) -> Self {
        NetlinkEndpoint {
            addr,
            addr_len
        }
    }
}