use crate::{filesystem::vfs::InodeId, net::socket};
use alloc::sync::Arc;

pub use smoltcp::wire::IpEndpoint;
pub use socket::netlink::endpoint::NetlinkEndpoint;

#[derive(Debug, Clone)]
pub enum Endpoint {
    /// 链路层端点
    LinkLayer(LinkLayerEndpoint),
    /// 网络层端点
    Ip(IpEndpoint),
    /// inode端点
    Inode(Arc<socket::Inode>),
    // todo: 增加NetLink机制后，增加NetLink端点
    InodeId(InodeId),
    /// NetLink端点
    Netlink(NetlinkEndpoint),
}

/// @brief 链路层端点
#[derive(Debug, Clone)]
pub struct LinkLayerEndpoint {
    /// 网卡的接口号
    pub interface: usize,
}

impl LinkLayerEndpoint {
    /// @brief 创建一个链路层端点
    ///
    /// @param interface 网卡的接口号
    ///
    /// @return 返回创建的链路层端点
    pub fn new(interface: usize) -> Self {
        Self { interface }
    }
}

impl From<IpEndpoint> for Endpoint {
    fn from(endpoint: IpEndpoint) -> Self {
        Self::Ip(endpoint)
    }
}
