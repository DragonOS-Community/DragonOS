use core::{
    fmt::{self, Debug},
    sync::atomic::AtomicUsize,
};

use alloc::{collections::BTreeMap, sync::Arc};

use crate::{driver::net::NetDevice, libs::rwlock::RwLock};
use smoltcp::wire::IpEndpoint;

use self::socket::SocketInode;

pub mod event_poll;
pub mod net_core;
pub mod socket;
pub mod syscall;

lazy_static! {
    /// # 所有网络接口的列表
    /// 这个列表在中断上下文会使用到，因此需要irqsave
    pub static ref NET_DEVICES: RwLock<BTreeMap<usize, Arc<dyn NetDevice>>> = RwLock::new(BTreeMap::new());
}

/// 生成网络接口的id (全局自增)
pub fn generate_iface_id() -> usize {
    static IFACE_ID: AtomicUsize = AtomicUsize::new(0);
    return IFACE_ID.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
}

bitflags! {
    /// @brief 用于指定socket的关闭类型
    /// 参考：https://code.dragonos.org.cn/xref/linux-6.1.9/include/net/sock.h?fi=SHUTDOWN_MASK#1573
    pub struct ShutdownType: u8 {
        const RCV_SHUTDOWN = 1;
        const SEND_SHUTDOWN = 2;
        const SHUTDOWN_MASK = 3;
    }
}

#[derive(Debug, Clone)]
pub enum Endpoint {
    /// 链路层端点
    LinkLayer(LinkLayerEndpoint),
    /// 网络层端点
    Ip(Option<IpEndpoint>),
    /// inode端点
    Inode(Option<Arc<SocketInode>>),
    // todo: 增加NetLink机制后，增加NetLink端点
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

/// IP datagram encapsulated protocol.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[repr(u8)]
pub enum Protocol {
    HopByHop = 0x00,
    Icmp = 0x01,
    Igmp = 0x02,
    Tcp = 0x06,
    Udp = 0x11,
    Ipv6Route = 0x2b,
    Ipv6Frag = 0x2c,
    Icmpv6 = 0x3a,
    Ipv6NoNxt = 0x3b,
    Ipv6Opts = 0x3c,
    Unknown(u8),
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Protocol::HopByHop => write!(f, "Hop-by-Hop"),
            Protocol::Icmp => write!(f, "ICMP"),
            Protocol::Igmp => write!(f, "IGMP"),
            Protocol::Tcp => write!(f, "TCP"),
            Protocol::Udp => write!(f, "UDP"),
            Protocol::Ipv6Route => write!(f, "IPv6-Route"),
            Protocol::Ipv6Frag => write!(f, "IPv6-Frag"),
            Protocol::Icmpv6 => write!(f, "ICMPv6"),
            Protocol::Ipv6NoNxt => write!(f, "IPv6-NoNxt"),
            Protocol::Ipv6Opts => write!(f, "IPv6-Opts"),
            Protocol::Unknown(id) => write!(f, "0x{id:02x}"),
        }
    }
}

impl From<smoltcp::wire::IpProtocol> for Protocol {
    fn from(value: smoltcp::wire::IpProtocol) -> Self {
        let x: u8 = value.into();
        Protocol::from(x)
    }
}

impl From<u8> for Protocol {
    fn from(value: u8) -> Self {
        match value {
            0x00 => Protocol::HopByHop,
            0x01 => Protocol::Icmp,
            0x02 => Protocol::Igmp,
            0x06 => Protocol::Tcp,
            0x11 => Protocol::Udp,
            0x2b => Protocol::Ipv6Route,
            0x2c => Protocol::Ipv6Frag,
            0x3a => Protocol::Icmpv6,
            0x3b => Protocol::Ipv6NoNxt,
            0x3c => Protocol::Ipv6Opts,
            _ => Protocol::Unknown(value),
        }
    }
}

impl From<Protocol> for u8 {
    fn from(value: Protocol) -> Self {
        match value {
            Protocol::HopByHop => 0x00,
            Protocol::Icmp => 0x01,
            Protocol::Igmp => 0x02,
            Protocol::Tcp => 0x06,
            Protocol::Udp => 0x11,
            Protocol::Ipv6Route => 0x2b,
            Protocol::Ipv6Frag => 0x2c,
            Protocol::Icmpv6 => 0x3a,
            Protocol::Ipv6NoNxt => 0x3b,
            Protocol::Ipv6Opts => 0x3c,
            Protocol::Unknown(id) => id,
        }
    }
}
