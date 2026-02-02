//! AF_INET/AF_INET6 SOCK_RAW 实现
//!
//! 提供 IP 层原始套接字支持，用于 ping、traceroute 等工具

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize};

use smoltcp::wire::{IpProtocol, IpVersion};

use crate::filesystem::vfs::{fasync::FAsyncItems, InodeId};
use crate::libs::mutex::Mutex;
use crate::libs::rwsem::RwSem;
use crate::libs::wait_queue::WaitQueue;
use crate::net::socket::common::EPollItems;
use crate::process::namespace::net_namespace::NetNamespace;

use inner::RawInner;

use self::loopback::LoopbackRxQueue;

use super::InetSocket;

mod constants;
pub mod inner;

mod loopback;
mod ops;
mod options;
mod packet;
mod recv;
mod send;
mod socket;
mod sockopt;

// 统一导出缓冲区/容量相关常量（Issue 7）。
#[allow(unused_imports)]
pub use constants::{SOCK_MIN_RCVBUF, SOCK_MIN_SNDBUF, SYSCTL_RMEM_MAX, SYSCTL_WMEM_MAX};

#[allow(unused_imports)]
pub use options::{Icmp6Filter, IcmpFilter, RawSocketOptions};

pub(crate) use loopback::deliver_udp_loopback_packet;

/// InetRawSocket - AF_INET/AF_INET6 SOCK_RAW 实现
///
/// 提供 IP 层原始套接字功能，支持：
/// - ICMP 协议 (ping)
/// - 自定义协议
/// - IP_HDRINCL 选项
/// - ICMP_FILTER 过滤
#[cast_to([sync] crate::net::socket::Socket)]
#[derive(Debug)]
pub struct RawSocket {
    /// 内部状态
    inner: RwSem<Option<RawInner>>,
    /// socket 选项
    options: RwSem<options::RawSocketOptions>,
    /// 非阻塞标志
    nonblock: AtomicBool,
    /// 等待队列
    wait_queue: WaitQueue,
    /// inode id
    inode_id: InodeId,
    /// 打开文件计数
    open_files: AtomicUsize,
    /// 自引用
    self_ref: Weak<Self>,
    /// 网络命名空间
    netns: Arc<NetNamespace>,
    /// epoll 项
    epoll_items: EPollItems,
    /// fasync 项
    fasync_items: FAsyncItems,
    /// IP 版本
    ip_version: IpVersion,
    /// 协议号
    protocol: IpProtocol,

    /// 回环快速路径：用于保留 TOS/TCLASS 等字段且实现 SO_RCVBUF 行为。
    loopback_rx: Mutex<LoopbackRxQueue>,

    /// IP_MULTICAST_IF: interface index
    ip_multicast_ifindex: AtomicI32,
    /// IP_MULTICAST_IF: interface address (network byte order)
    ip_multicast_addr: AtomicU32,
    /// IP_ADD_MEMBERSHIP/IP_DROP_MEMBERSHIP state (best-effort, no actual IGMP)
    ip_multicast_groups: Mutex<Vec<crate::net::socket::inet::common::Ipv4MulticastMembership>>,
}
