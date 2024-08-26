use core::{
    fmt::{self, Debug},
    sync::atomic::AtomicUsize,
};

use alloc::{collections::BTreeMap, sync::Arc};
use socket::netlink::endpoint::NetlinkEndpoint;
use socket::Socket;

use crate::{driver::net::Iface, libs::rwlock::RwLock};
use smoltcp::wire::IpEndpoint;

pub mod event_poll;
pub mod net_core;
pub mod socket;
pub mod syscall;

lazy_static! {
    /// # 所有网络接口的列表
    /// 这个列表在中断上下文会使用到，因此需要irqsave
    pub static ref NET_DEVICES: RwLock<BTreeMap<usize, Arc<dyn Iface>>> = RwLock::new(BTreeMap::new());
}

/// 生成网络接口的id (全局自增)
pub fn generate_iface_id() -> usize {
    static IFACE_ID: AtomicUsize = AtomicUsize::new(0);
    return IFACE_ID.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
}

#[derive(Debug, Clone)]
pub enum Endpoint {
    /// 链路层端点
    LinkLayer(LinkLayerEndpoint),
    /// 网络层端点
    Ip(IpEndpoint),
    /// inode端点
    Inode(Arc<dyn Socket>),
    // todo: 增加NetLink机制后，增加NetLink端点
    /// NetLink端点
    Netlink(Option<NetlinkEndpoint>),
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