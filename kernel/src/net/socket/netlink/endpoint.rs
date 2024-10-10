use crate::net::syscall::SockAddrNl;
#[derive(Debug, Clone)]
pub struct NetlinkEndpoint {
    pub addr: SockAddrNl,
}
impl NetlinkEndpoint {
    pub fn new(addr: SockAddrNl) -> Self {
        NetlinkEndpoint { addr }
    }
}
