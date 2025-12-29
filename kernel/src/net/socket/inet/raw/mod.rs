//! AF_INET/AF_INET6 SOCK_RAW 实现
//!
//! 提供 IP 层原始套接字支持，用于 ping、traceroute 等工具

use inner::{RawInner, UnboundRaw};
use smoltcp::wire::{IpAddress, IpProtocol, IpVersion, Ipv4Address, Ipv4Packet, Ipv6Packet};
use system_error::SystemError;

use crate::filesystem::epoll::EPollEventType;
use crate::filesystem::vfs::iov::IoVecs;
use crate::filesystem::vfs::{fasync::FAsyncItems, vcore::generate_inode_id, InodeId};
use crate::libs::rwlock::RwLock;
use crate::libs::spinlock::SpinLock;
use crate::libs::wait_queue::WaitQueue;
use crate::net::posix::SockAddr;
use crate::net::socket::common::EPollItems;
use crate::net::socket::endpoint::Endpoint;
use crate::net::socket::unix::utils::{cmsg_align, CmsgBuffer, Cmsghdr};
use crate::net::socket::utils::{IPV4_MIN_HEADER_LEN, IPV6_HEADER_LEN};
use crate::net::socket::{Socket, IFNAMSIZ, PIP, PIPV6, PMSG, PRAW, PSO, PSOL};
use crate::process::cred::CAPFlags;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::namespace::NamespaceOps;
use crate::process::ProcessManager;
use crate::process::ProcessState;
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::InetSocket;

mod constants;
pub mod inner;

// 统一导出缓冲区/容量相关常量（Issue 7）。
pub use constants::{SOCK_MIN_RCVBUF, SOCK_MIN_SNDBUF, SYSCTL_RMEM_MAX, SYSCTL_WMEM_MAX};

type EP = crate::filesystem::epoll::EPollEventType;

/// ICMP 过滤器 (32位位掩码)
///
/// 用于过滤特定 ICMP 类型的数据包
#[derive(Debug, Clone, Default)]
pub struct IcmpFilter {
    /// 位掩码：第 N 位为 1 表示过滤掉 ICMP type N (0-31)
    mask: u32,
}

impl IcmpFilter {
    #[allow(dead_code)]
    pub fn new(mask: u32) -> Self {
        Self { mask }
    }

    /// 检查是否应该过滤该 ICMP 类型
    ///
    /// # 参数
    /// - `icmp_type`: ICMP 消息类型 (0-255)
    ///
    /// # 返回
    /// - `true`: 应该过滤掉（丢弃）
    /// - `false`: 应该接收
    pub fn should_filter(&self, icmp_type: u8) -> bool {
        if icmp_type >= 32 {
            return false; // 超出范围的类型不过滤
        }
        (self.mask & (1 << icmp_type)) != 0
    }

    pub fn set_mask(&mut self, mask: u32) {
        self.mask = mask;
    }

    pub fn get_mask(&self) -> u32 {
        self.mask
    }
}

/// Raw socket 选项
#[derive(Debug, Clone)]
pub struct RawSocketOptions {
    /// IP_HDRINCL: 用户是否提供完整 IP 头
    pub ip_hdrincl: bool,
    /// IP_TOS: Type of Service
    pub ip_tos: u8,
    /// IP_TTL: Time to Live
    pub ip_ttl: u8,
    /// IP_PKTINFO: 接收时是否返回 in_pktinfo 控制消息
    pub recv_pktinfo_v4: bool,
    /// IP_RECVTOS: 接收时是否返回 IP_TOS 控制消息
    pub recv_tos: bool,
    /// IP_RECVTTL: 接收时是否返回 IP_TTL 控制消息
    pub recv_ttl: bool,
    /// IPV6_RECVPKTINFO: 接收时是否返回 in6_pktinfo 控制消息
    pub recv_pktinfo_v6: bool,
    /// IPV6_RECVTCLASS: 接收时是否返回 IPV6_TCLASS 控制消息
    pub recv_tclass: bool,
    /// IPV6_RECVHOPLIMIT: 接收时是否返回 IPV6_HOPLIMIT 控制消息
    pub recv_hoplimit: bool,
    /// ICMP_FILTER: ICMP 类型过滤位掩码 (仅 IPPROTO_ICMP)
    pub icmp_filter: IcmpFilter,

    /// IPV6_CHECKSUM: -1 表示不校验/不计算；否则为校验和字段在 payload 内的偏移(单位：字节)
    pub ipv6_checksum: i32,

    /// SO_SNDBUF: 返回给 getsockopt 的 sk_sndbuf（Linux 会将 setsockopt 的值 *2 后存储）
    pub sock_sndbuf: u32,
    /// SO_RCVBUF: 返回给 getsockopt 的 sk_rcvbuf（Linux 会将 setsockopt 的值 *2 后存储）
    pub sock_rcvbuf: u32,

    /// SO_BINDTODEVICE: 绑定的设备名（不含 '\0'）
    pub bind_to_device: Option<String>,

    /// SO_ATTACH_FILTER/ SO_DETACH_FILTER
    pub filter_attached: bool,
}

impl Default for RawSocketOptions {
    fn default() -> Self {
        Self {
            ip_hdrincl: false,
            ip_tos: 0,
            ip_ttl: DEFAULT_IP_TTL,
            recv_pktinfo_v4: false,
            recv_tos: false,
            recv_ttl: false,
            recv_pktinfo_v6: false,
            recv_tclass: false,
            recv_hoplimit: false,
            icmp_filter: IcmpFilter::default(),
            ipv6_checksum: -1,

            // Linux 语义：内核存储的 sk_sndbuf/sk_rcvbuf 是用户设置值的 2 倍。
            // 初始值设为 sysctl_*mem_max * 2，与 Linux 默认行为一致。
            sock_sndbuf: SYSCTL_WMEM_MAX.saturating_mul(2),
            sock_rcvbuf: SYSCTL_RMEM_MAX.saturating_mul(2),
            bind_to_device: None,
            filter_attached: false,
        }
    }
}

// ============================================================================
// 常量定义
// ============================================================================

/// 默认 IP TTL/Hop Limit (RFC 1340)
const DEFAULT_IP_TTL: u8 = 64;

// SOL_SOCKET 层选项 (include/uapi/asm-generic/socket.h)
// SO_* / IP_* / RAW_* 选项号使用公共枚举 PSO/PIP/PRAW（避免重复定义）。

// SKB 内存计费常量 (参考 Linux 6.6 include/linux/skbuff.h)
/// SKB 数据对齐大小 (SMP_CACHE_BYTES)
const SKB_DATA_ALIGN: usize = 64;
/// SKB 管理开销 (sizeof(sk_buff) + sizeof(skb_shared_info) 对齐后)
const SKB_OVERHEAD: usize = 576;

#[derive(Debug, Default)]
struct LoopbackRxQueue {
    pkts: VecDeque<Vec<u8>>,
    bytes: usize,
}

/// IP 包构造参数
struct IpPacketParams<'a> {
    payload: &'a [u8],
    src: IpAddress,
    dst: IpAddress,
    protocol: IpProtocol,
    ttl: u8,
    tos: u8,
    ipv6_checksum: i32,
}

/// 构造 IPv4 数据包
fn build_ipv4_packet(params: &IpPacketParams) -> Result<Vec<u8>, SystemError> {
    // IPv4 total length is u16.
    if params
        .payload
        .len()
        .checked_add(IPV4_MIN_HEADER_LEN)
        .filter(|v| *v <= u16::MAX as usize)
        .is_none()
    {
        return Err(SystemError::EMSGSIZE);
    }

    let dst = match params.dst {
        IpAddress::Ipv4(v) => v,
        _ => return Err(SystemError::EAFNOSUPPORT),
    };

    let src = match params.src {
        IpAddress::Ipv4(v) => v,
        _ => return Err(SystemError::EAFNOSUPPORT),
    };

    let mut bytes = vec![0u8; IPV4_MIN_HEADER_LEN + params.payload.len()];
    let mut pkt = Ipv4Packet::new_unchecked(&mut bytes);
    pkt.set_version(4);
    pkt.set_header_len(IPV4_MIN_HEADER_LEN as u8);
    pkt.set_total_len((IPV4_MIN_HEADER_LEN + params.payload.len()) as u16);
    pkt.set_ident(0);
    pkt.clear_flags();
    pkt.set_frag_offset(0);
    pkt.set_hop_limit(params.ttl);
    pkt.set_next_header(params.protocol);
    pkt.set_src_addr(src);
    pkt.set_dst_addr(dst);
    pkt.set_dscp(params.tos >> 2);
    pkt.set_ecn(params.tos & 0x3);
    pkt.payload_mut()[..params.payload.len()].copy_from_slice(params.payload);
    pkt.fill_checksum();
    Ok(bytes)
}

/// 构造 IPv6 数据包
fn build_ipv6_packet(params: &IpPacketParams) -> Result<Vec<u8>, SystemError> {
    // IPv6 payload length is u16; reject jumbograms.
    if params.payload.len() > u16::MAX as usize {
        return Err(SystemError::EMSGSIZE);
    }

    let dst = match params.dst {
        IpAddress::Ipv6(v) => v,
        _ => return Err(SystemError::EAFNOSUPPORT),
    };

    let src = match params.src {
        IpAddress::Ipv6(v) => v,
        _ => return Err(SystemError::EAFNOSUPPORT),
    };

    let mut bytes = vec![0u8; IPV6_HEADER_LEN + params.payload.len()];
    let mut pkt = Ipv6Packet::new_unchecked(&mut bytes);
    pkt.set_version(6);
    pkt.set_traffic_class(params.tos);
    pkt.set_flow_label(0);
    pkt.set_payload_len(params.payload.len() as u16);
    pkt.set_next_header(params.protocol);
    pkt.set_hop_limit(params.ttl);
    pkt.set_src_addr(src);
    pkt.set_dst_addr(dst);
    pkt.payload_mut()[..params.payload.len()].copy_from_slice(params.payload);

    // Linux 语义：当设置 IPV6_CHECKSUM 且协议为 UDP 时，内核负责计算并填充校验和。
    if params.protocol == IpProtocol::Udp && params.ipv6_checksum >= 0 {
        let off = params.ipv6_checksum as usize;
        if !off.is_multiple_of(2) || off + 2 > params.payload.len() {
            return Err(SystemError::EINVAL);
        }
        let xsum = ipv6_udp_checksum(&bytes, off).ok_or(SystemError::EINVAL)?;
        let payload = &mut bytes[IPV6_HEADER_LEN..];
        payload[off..off + 2].copy_from_slice(&xsum.to_be_bytes());
    }
    Ok(bytes)
}

/// 构造 IP 数据包（根据 IP 版本自动选择）
fn build_ip_packet(ip_version: IpVersion, params: &IpPacketParams) -> Result<Vec<u8>, SystemError> {
    match ip_version {
        IpVersion::Ipv4 => build_ipv4_packet(params),
        IpVersion::Ipv6 => build_ipv6_packet(params),
    }
}

/// 检查目标地址是否为 loopback
#[inline]
fn is_loopback_addr(addr: IpAddress) -> bool {
    match addr {
        IpAddress::Ipv4(v4) => v4.is_loopback(),
        IpAddress::Ipv6(v6) => v6.is_loopback(),
    }
}

/// Loopback 投递上下文
struct LoopbackDeliverContext<'a> {
    packet: &'a [u8],
    dest: IpAddress,
    ip_version: IpVersion,
    protocol: IpProtocol,
    netns: &'a Arc<NetNamespace>,
}

/// 向同一 netns 下所有匹配的 raw socket 投递 loopback 数据包
fn deliver_loopback_packet(ctx: &LoopbackDeliverContext) {
    let sockets = raw_sockets_in_netns(ctx.netns);
    let pkt_cost = loopback_rx_mem_cost(ctx.packet.len());

    for s in sockets.iter() {
        if s.ip_version != ctx.ip_version || s.protocol != ctx.protocol {
            continue;
        }

        // SO_BINDTODEVICE：loopback 快速路径视为来自 lo。
        if let Some(dev) = &s.options.read().bind_to_device {
            if dev.as_str() != "lo" {
                continue;
            }
        }

        // bind(2) 目的地址过滤：仅在 local_addr 指定时生效。
        let local = match s.inner.read().as_ref() {
            Some(RawInner::Bound(b) | RawInner::Wildcard(b)) => b.local_addr(),
            _ => None,
        };
        if let Some(local) = local {
            if local != ctx.dest {
                continue;
            }
        }

        // IPV6_CHECKSUM 接收校验：对启用了 IPV6_CHECKSUM 的 socket，丢弃校验失败的 UDP/IPv6 包。
        if s.ip_version == IpVersion::Ipv6 && s.protocol == IpProtocol::Udp {
            let off = s.options.read().ipv6_checksum;
            if off >= 0 {
                let off = off as usize;
                if ctx.packet.len() < IPV6_HEADER_LEN {
                    continue;
                }
                let payload = &ctx.packet[IPV6_HEADER_LEN..];
                if off + 2 > payload.len() {
                    continue;
                }
                let got = u16::from_be_bytes([payload[off], payload[off + 1]]);
                if got == 0 {
                    continue;
                }
                match ipv6_udp_checksum(ctx.packet, off) {
                    Some(expect) if expect == got => {}
                    _ => continue,
                }
            }
        }

        // SO_RCVBUF：投递/丢弃语义。
        let rcvbuf = s.options.read().sock_rcvbuf as usize;
        let enqueued = {
            let mut q = s.loopback_rx.lock_irqsave();
            let can_enqueue = if q.bytes == 0 {
                // Linux/Netstack：当接收队列为空时，允许接收一个超过 rcvbuf 的 dgram。
                true
            } else {
                q.bytes.saturating_add(pkt_cost) <= rcvbuf
            };
            if can_enqueue {
                q.bytes = q.bytes.saturating_add(pkt_cost);
                q.pkts.push_back(ctx.packet.to_vec());
            }
            can_enqueue
        };

        if enqueued {
            s.notify();
            let _ = s.wait_queue.wakeup(Some(ProcessState::Blocked(true)));
        }
    }
}

#[inline]
fn loopback_rx_mem_cost(pkt_len: usize) -> usize {
    // Linux 的 sk_rmem_alloc 按 skb->truesize（含管理开销与对齐）计费，
    // 不是仅按 payload/IP 包长度计费。gVisor 的 RecvBufLimits 也依赖此语义：
    // 在 rcvbuf = 4 * min 时，第 4 个 min 包会因为额外开销而被丢弃。
    // 这里实现一个足够接近 Linux 6.6 行为的近似：
    // - 对齐：SKB_DATA_ALIGN(X) = ALIGN(X, SMP_CACHE_BYTES)（常见为 64）
    // - 开销：SKB_TRUESIZE(X) 还会叠加 data_align(sizeof(sk_buff)) + data_align(sizeof(skb_shared_info))
    // 由于 DragonOS 不复用 Linux 的 skb，这里用常见的合计开销近似（约 576B）。
    let aligned = (pkt_len + (SKB_DATA_ALIGN - 1)) & !(SKB_DATA_ALIGN - 1);
    aligned.saturating_add(SKB_OVERHEAD)
}

fn sock_buf_u32_from_opt(val: &[u8]) -> Result<u32, SystemError> {
    if val.len() < 4 {
        return Err(SystemError::EINVAL);
    }
    Ok(u32::from_ne_bytes([val[0], val[1], val[2], val[3]]))
}

fn clamp_sock_buf(val_u32: u32, sysctl_max: u32, sock_min: u32) -> u32 {
    // Linux: val = min_t(u32, val, sysctl_*mem_max)
    let mut val = core::cmp::min(val_u32, sysctl_max);
    // Ensure val*2 won't overflow signed int logic.
    val = core::cmp::min(val, (i32::MAX as u32) / 2);
    let doubled = val.saturating_mul(2);
    core::cmp::max(doubled, sock_min)
}

fn read_i32_opt(val: &[u8]) -> Option<i32> {
    if val.len() >= 4 {
        Some(i32::from_ne_bytes([val[0], val[1], val[2], val[3]]))
    } else {
        None
    }
}

fn checksum_add(sum: &mut u32, data: &[u8]) {
    let mut i = 0usize;
    while i + 1 < data.len() {
        *sum = sum.wrapping_add(u16::from_be_bytes([data[i], data[i + 1]]) as u32);
        i += 2;
    }
    if i < data.len() {
        *sum = sum.wrapping_add(u16::from_be_bytes([data[i], 0]) as u32);
    }
}

fn checksum_finish(mut sum: u32) -> u16 {
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff).wrapping_add(sum >> 16);
    }
    let out = !(sum as u16);
    if out == 0 {
        0xffff
    } else {
        out
    }
}

fn ipv6_udp_checksum(packet: &[u8], checksum_off_in_payload: usize) -> Option<u16> {
    if packet.len() < 40 {
        return None;
    }
    let payload = &packet[40..];
    if checksum_off_in_payload + 2 > payload.len() {
        return None;
    }
    if !checksum_off_in_payload.is_multiple_of(2) {
        return None;
    }

    let src = &packet[8..24];
    let dst = &packet[24..40];
    let payload_len = payload.len() as u32;

    let mut sum: u32 = 0;
    checksum_add(&mut sum, src);
    checksum_add(&mut sum, dst);
    checksum_add(&mut sum, &payload_len.to_be_bytes());
    checksum_add(&mut sum, &[0, 0, 0, 17]); // IPPROTO_UDP

    // Upper-layer packet with checksum field zeroed.
    checksum_add(&mut sum, &payload[..checksum_off_in_payload]);
    checksum_add(&mut sum, &[0, 0]);
    checksum_add(&mut sum, &payload[checksum_off_in_payload + 2..]);
    Some(checksum_finish(sum))
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct InPktInfo {
    ipi_ifindex: i32,
    ipi_spec_dst: u32,
    ipi_addr: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct In6PktInfo {
    ipi6_addr: [u8; 16],
    ipi6_ifindex: u32,
}

// cmsg level 使用公共枚举 PSOL（其值与 Linux SOL_* 一致）。

// Linux UAPI (include/uapi/linux/in6.h) 使用公共枚举 PIPV6。

/// InetRawSocket - AF_INET/AF_INET6 SOCK_RAW 实现
///
/// 提供 IP 层原始套接字功能，支持：
/// - ICMP 协议 (ping)
/// - 自定义协议
/// - IP_HDRINCL 选项
/// - ICMP_FILTER 过滤
#[cast_to([sync] Socket)]
#[derive(Debug)]
pub struct RawSocket {
    /// 内部状态
    inner: RwLock<Option<RawInner>>,
    /// socket 选项
    options: RwLock<RawSocketOptions>,
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
    loopback_rx: SpinLock<LoopbackRxQueue>,
}

lazy_static! {
    /// 同一 netns 下的 raw socket 列表（用于 loopback 快速路径广播投递）。
    static ref RAW_SOCKET_REGISTRY: RwLock<BTreeMap<usize, Vec<Weak<RawSocket>>>> =
        RwLock::new(BTreeMap::new());
}

fn register_raw_socket(sock: &Arc<RawSocket>) {
    let netns_id = sock.netns.ns_common().nsid.data();
    let mut reg = RAW_SOCKET_REGISTRY.write();
    let entry = reg.entry(netns_id).or_default();
    entry.push(Arc::downgrade(sock));
    // 轻量清理：避免条目无限增长。
    entry.retain(|w| w.upgrade().is_some());
}

fn raw_sockets_in_netns(netns: &Arc<NetNamespace>) -> Vec<Arc<RawSocket>> {
    let netns_id = netns.ns_common().nsid.data();

    // 直接使用写锁，避免读-写锁升级的竞态
    let mut reg = RAW_SOCKET_REGISTRY.write();
    let entry = match reg.get_mut(&netns_id) {
        Some(v) => v,
        None => return Vec::new(),
    };

    let mut result = Vec::new();
    entry.retain(|w| {
        if let Some(s) = w.upgrade() {
            result.push(s);
            true
        } else {
            false
        }
    });

    if entry.is_empty() {
        reg.remove(&netns_id);
    }

    result
}

impl RawSocket {
    /// 创建新的 raw socket
    ///
    /// # 权限检查
    /// 需要 CAP_NET_RAW 权限
    ///
    /// # 参数
    /// - `ip_version`: IP 版本 (IPv4 或 IPv6)
    /// - `protocol`: IP 协议号
    /// - `nonblock`: 是否非阻塞
    ///
    /// # 返回
    /// - `Ok(Arc<Self>)`: 成功创建的 raw socket
    /// - `Err(SystemError::EPERM)`: 没有 CAP_NET_RAW 权限
    pub fn new(
        ip_version: IpVersion,
        protocol: IpProtocol,
        nonblock: bool,
    ) -> Result<Arc<Self>, SystemError> {
        // CAP_NET_RAW 权限检查
        let cred = ProcessManager::current_pcb().cred();
        if !cred.has_capability(CAPFlags::CAP_NET_RAW) {
            log::warn!("RawSocket::new: CAP_NET_RAW check failed");
            return Err(SystemError::EPERM);
        }

        let netns = ProcessManager::current_netns();

        // IPPROTO_RAW (255) 自动启用 IP_HDRINCL
        let ip_hdrincl = protocol == IpProtocol::Unknown(255);

        let options = RawSocketOptions {
            ip_hdrincl,
            ..Default::default()
        };

        // Linux 语义：raw socket 创建时不要求必须存在网卡/路由。
        // 但为了让未 bind 的 raw socket 能接收数据包、且 poll/epoll 能正确唤醒，
        // 在存在 iface 时优先以“通配接收”方式附着到 loopback/默认 iface。
        // 若当前 netns 尚无可用 iface，则退化为 Unbound，允许后续 bind/connect/sendto 再完成选址与附着。
        let initial_inner = match UnboundRaw::new(ip_version, protocol).bind_wildcard(netns.clone())
        {
            Ok(wildcard) => RawInner::Wildcard(wildcard),
            Err(SystemError::ENODEV) => RawInner::Unbound(UnboundRaw::new(ip_version, protocol)),
            Err(e) => return Err(e),
        };

        let sock = Arc::new_cyclic(|me| Self {
            inner: RwLock::new(Some(initial_inner)),
            options: RwLock::new(options),
            nonblock: AtomicBool::new(nonblock),
            wait_queue: WaitQueue::default(),
            inode_id: generate_inode_id(),
            open_files: AtomicUsize::new(0),
            self_ref: me.clone(),
            netns,
            epoll_items: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
            ip_version,
            protocol,
            loopback_rx: SpinLock::new(LoopbackRxQueue::default()),
        });

        // Linux 语义：raw socket 未 bind 也应能被 poll/epoll 正确唤醒。
        // IfaceCommon::poll() 只会对注册在 bounds 列表里的 inet sockets 做 notify/wakeup，
        // 因此 wildcard 状态下需要注册到对应 iface。
        if let Some(RawInner::Wildcard(bound)) = sock.inner.read().as_ref() {
            bound.inner().iface().common().bind_socket(sock.clone());
        }

        register_raw_socket(&sock);

        Ok(sock)
    }

    pub fn is_nonblock(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    #[inline]
    fn protocol_u16(&self) -> u16 {
        // Linux raw socket uses sockaddr_in{,6}.sin_port to carry the protocol number.
        // gVisor raw_socket_test expects this behavior.
        match self.protocol {
            IpProtocol::HopByHop => 0,
            IpProtocol::Icmp => 1,
            IpProtocol::Igmp => 2,
            IpProtocol::Tcp => 6,
            IpProtocol::Udp => 17,
            IpProtocol::Ipv6Route => 43,
            IpProtocol::Ipv6Frag => 44,
            IpProtocol::Icmpv6 => 58,
            IpProtocol::Ipv6NoNxt => 59,
            IpProtocol::Ipv6Opts => 60,
            IpProtocol::Unknown(v) => v as u16,
            _ => 0,
        }
    }

    #[inline]
    pub fn is_ipv6(&self) -> bool {
        self.ip_version == IpVersion::Ipv6
    }

    #[inline]
    fn addr_matches_ip_version(&self, addr: smoltcp::wire::IpAddress) -> bool {
        matches!(
            (self.ip_version, addr),
            (IpVersion::Ipv4, smoltcp::wire::IpAddress::Ipv4(_))
                | (IpVersion::Ipv6, smoltcp::wire::IpAddress::Ipv6(_))
        )
    }

    /// 绑定到本地地址
    pub fn do_bind(&self, local_addr: smoltcp::wire::IpAddress) -> Result<(), SystemError> {
        if !self.addr_matches_ip_version(local_addr) {
            return Err(SystemError::EAFNOSUPPORT);
        }
        let mut inner = self.inner.write();
        let prev = inner.take().ok_or(SystemError::EINVAL)?;
        match prev {
            RawInner::Unbound(unbound) => match unbound.bind(local_addr, self.netns.clone()) {
                Ok(bound) => {
                    bound
                        .inner()
                        .iface()
                        .common()
                        .bind_socket(self.self_ref.upgrade().unwrap());
                    *inner = Some(RawInner::Bound(bound));
                    Ok(())
                }
                Err(e) => {
                    // bind 消费了 unbound（move）。失败时恢复为新的 Unbound 状态，
                    // 避免 inner=None 导致后续 unwrap panic。
                    *inner = Some(RawInner::Unbound(UnboundRaw::new(
                        self.ip_version,
                        self.protocol,
                    )));
                    // Linux 语义：绑定到不存在的本地地址应返回 EADDRNOTAVAIL。
                    Err(if matches!(e, SystemError::ENODEV) {
                        SystemError::EADDRNOTAVAIL
                    } else {
                        e
                    })
                }
            },
            RawInner::Wildcard(wildcard) => {
                // 从通配接收状态切换为用户显式绑定：先释放旧 handle，再按地址绑定。
                wildcard.close();
                let unbound = UnboundRaw::new(self.ip_version, self.protocol);
                match unbound.bind(local_addr, self.netns.clone()) {
                    Ok(bound) => {
                        bound
                            .inner()
                            .iface()
                            .common()
                            .bind_socket(self.self_ref.upgrade().unwrap());
                        *inner = Some(RawInner::Bound(bound));
                        Ok(())
                    }
                    Err(e) => {
                        // 失败则回到通配接收（Linux 语义下不应让 socket 进入不可用状态）
                        let wildcard = UnboundRaw::new(self.ip_version, self.protocol)
                            .bind_wildcard(self.netns.clone())?;
                        *inner = Some(RawInner::Wildcard(wildcard));
                        Err(if matches!(e, SystemError::ENODEV) {
                            SystemError::EADDRNOTAVAIL
                        } else {
                            e
                        })
                    }
                }
            }
            other => {
                *inner = Some(other);
                Err(SystemError::EINVAL)
            }
        }
    }

    /// 绑定到临时地址（根据远程地址选择合适的本地地址）
    pub fn bind_ephemeral(&self, remote: smoltcp::wire::IpAddress) -> Result<(), SystemError> {
        if !self.addr_matches_ip_version(remote) {
            return Err(SystemError::EAFNOSUPPORT);
        }
        let mut inner_guard = self.inner.write();
        let prev = inner_guard.take().ok_or(SystemError::EINVAL)?;
        match prev {
            RawInner::Bound(bound) => {
                inner_guard.replace(RawInner::Bound(bound));
                Ok(())
            }
            RawInner::Wildcard(wildcard) => {
                // Wildcard 仅表示已附着到某个 iface；为符合 Linux 语义（connect/getSockName），
                // 这里需要真正选址并记录 local_addr。
                wildcard.close();
                let unbound = UnboundRaw::new(self.ip_version, self.protocol);
                match unbound.bind_ephemeral(remote, self.netns.clone()) {
                    Ok(bound) => {
                        bound
                            .inner()
                            .iface()
                            .common()
                            .bind_socket(self.self_ref.upgrade().unwrap());
                        inner_guard.replace(RawInner::Bound(bound));
                        Ok(())
                    }
                    Err(e) => {
                        // 失败则恢复为通配接收，避免 socket 进入不可用状态。
                        let wildcard = UnboundRaw::new(self.ip_version, self.protocol)
                            .bind_wildcard(self.netns.clone())?;
                        inner_guard.replace(RawInner::Wildcard(wildcard));
                        Err(e)
                    }
                }
            }
            RawInner::Unbound(unbound) => {
                match unbound.bind_ephemeral(remote, self.netns.clone()) {
                    Ok(bound) => {
                        bound
                            .inner()
                            .iface()
                            .common()
                            .bind_socket(self.self_ref.upgrade().unwrap());
                        inner_guard.replace(RawInner::Bound(bound));
                        Ok(())
                    }
                    Err(e) => {
                        // bind_ephemeral 消费了 unbound（move），失败恢复为 Unbound。
                        inner_guard.replace(RawInner::Unbound(UnboundRaw::new(
                            self.ip_version,
                            self.protocol,
                        )));
                        Err(e)
                    }
                }
            }
        }
    }

    pub fn is_bound(&self) -> bool {
        let inner = self.inner.read();
        matches!(&*inner, Some(RawInner::Bound(_) | RawInner::Wildcard(_)))
    }

    pub fn close(&self) {
        let mut inner = self.inner.write();
        match &mut *inner {
            Some(RawInner::Bound(bound)) => {
                bound.close();
                inner.take();
            }
            Some(RawInner::Wildcard(bound)) => {
                bound.close();
                inner.take();
            }
            _ => {}
        }
    }

    /// 尝试接收数据包
    ///
    /// # 返回
    /// - `Ok((size, src_addr))`: 接收到的数据大小和源地址
    /// - `Err(SystemError::EAGAIN_OR_EWOULDBLOCK)`: 没有数据可读
    pub fn try_recv(
        &self,
        buf: &mut [u8],
    ) -> Result<(usize, smoltcp::wire::IpAddress), SystemError> {
        // 先消费回环注入队列（保留原始头字段，并实现 SO_RCVBUF 语义）。
        if let Some(pkt) = {
            let mut q = self.loopback_rx.lock_irqsave();
            let pkt = q.pkts.pop_front();
            if let Some(ref p) = pkt {
                q.bytes = q.bytes.saturating_sub(loopback_rx_mem_cost(p.len()));
            }
            pkt
        } {
            let len = pkt.len().min(buf.len());
            buf[..len].copy_from_slice(&pkt[..len]);
            let src_addr = match self.ip_version {
                IpVersion::Ipv4 => {
                    if pkt.len() >= 20 {
                        IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(
                            pkt[12], pkt[13], pkt[14], pkt[15],
                        ))
                    } else {
                        IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED)
                    }
                }
                IpVersion::Ipv6 => {
                    if pkt.len() >= 40 {
                        let b: [u8; 16] = pkt[8..24].try_into().unwrap_or([0; 16]);
                        IpAddress::Ipv6(smoltcp::wire::Ipv6Address::new(
                            u16::from_be_bytes([b[0], b[1]]),
                            u16::from_be_bytes([b[2], b[3]]),
                            u16::from_be_bytes([b[4], b[5]]),
                            u16::from_be_bytes([b[6], b[7]]),
                            u16::from_be_bytes([b[8], b[9]]),
                            u16::from_be_bytes([b[10], b[11]]),
                            u16::from_be_bytes([b[12], b[13]]),
                            u16::from_be_bytes([b[14], b[15]]),
                        ))
                    } else {
                        IpAddress::Ipv6(smoltcp::wire::Ipv6Address::UNSPECIFIED)
                    }
                }
            };
            return Ok((len, src_addr));
        }

        let inner_guard = self.inner.read();
        match inner_guard.as_ref() {
            None => Err(SystemError::ENOTCONN),
            Some(RawInner::Bound(bound)) => {
                // 接收数据，并按 Linux 语义应用 bind(2) 的目的地址过滤。
                // gVisor raw_socket_test: RawSocketTest.BindReceive
                // 添加最大重试次数限制，避免大量不匹配包导致的 busy-wait。
                const MAX_FILTER_RETRIES: usize = 64;
                let mut result;
                let mut retries = 0;
                loop {
                    result = bound.try_recv(buf, self.ip_version);
                    match &result {
                        Ok((size, _src_addr)) => {
                            if let Some(local) = bound.local_addr() {
                                if let Some(dst) = inner::extract_dst_addr_from_ip_header(
                                    &buf[..(*size).min(buf.len())],
                                    self.ip_version,
                                ) {
                                    if dst != local {
                                        // 丢弃不匹配的包，继续尝试读取下一包。
                                        retries += 1;
                                        if retries >= MAX_FILTER_RETRIES {
                                            result = Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                                            break;
                                        }
                                        continue;
                                    }
                                }
                            }

                            // Linux 语义：若启用了 IPV6_CHECKSUM（用于 UDP），则在接收路径校验校验和。
                            if self.ip_version == IpVersion::Ipv6
                                && self.protocol == IpProtocol::Udp
                            {
                                let off = self.options.read().ipv6_checksum;
                                if off >= 0 {
                                    let size = (*size).min(buf.len());
                                    let packet = &buf[..size];
                                    let off = off as usize;
                                    if packet.len() < 40 {
                                        retries += 1;
                                        if retries >= MAX_FILTER_RETRIES {
                                            result = Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                                            break;
                                        }
                                        continue;
                                    }
                                    let payload = &packet[40..];
                                    if off + 2 > payload.len() {
                                        retries += 1;
                                        if retries >= MAX_FILTER_RETRIES {
                                            result = Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                                            break;
                                        }
                                        continue;
                                    }
                                    let got = u16::from_be_bytes([payload[off], payload[off + 1]]);
                                    // IPv6/UDP: checksum 不能为 0。
                                    if got == 0 {
                                        retries += 1;
                                        if retries >= MAX_FILTER_RETRIES {
                                            result = Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                                            break;
                                        }
                                        continue;
                                    }
                                    match ipv6_udp_checksum(packet, off) {
                                        Some(expect) if expect == got => {}
                                        _ => {
                                            retries += 1;
                                            if retries >= MAX_FILTER_RETRIES {
                                                result = Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                                                break;
                                            }
                                            continue;
                                        }
                                    }
                                }
                            }
                            break;
                        }
                        Err(_) => break,
                    }
                }

                // 应用 ICMP_FILTER
                if let Ok((size, src_addr)) = &result {
                    if self.protocol == IpProtocol::Icmp && *size > 0 {
                        // 获取 ICMP type (IP 头后第一个字节)
                        let ip_header_len = self.get_ip_header_len(buf);
                        if buf.len() > ip_header_len {
                            let icmp_type = buf[ip_header_len];
                            if self.options.read().icmp_filter.should_filter(icmp_type) {
                                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                            }
                        }
                    }
                    // 触发 poll
                    bound.inner().iface().poll();
                    return Ok((*size, *src_addr));
                }
                result
            }
            Some(RawInner::Wildcard(bound)) => {
                let result = bound.try_recv(buf, self.ip_version);
                if let Ok((_size, _src_addr)) = &result {
                    bound.inner().iface().poll();
                }
                result
            }
            Some(RawInner::Unbound(_)) => Err(SystemError::ENOTCONN),
        }
    }

    /// Linux 语义：IPv4 raw socket 的 recv/recvfrom 返回整个 IPv4 包（含 IPv4 头）；
    /// IPv6 raw socket 默认只返回 payload（不含 IPv6 固定头）。
    fn try_recv_user(
        &self,
        user_buf: &mut [u8],
    ) -> Result<(usize, smoltcp::wire::IpAddress), SystemError> {
        match self.ip_version {
            IpVersion::Ipv4 => self.try_recv(user_buf),
            IpVersion::Ipv6 => {
                // 需要先接收完整 IPv6 包，用于解析 src addr / cmsg，随后只向用户拷贝 payload。
                let mut tmp = vec![
                    0u8;
                    user_buf
                        .len()
                        .saturating_add(IPV6_HEADER_LEN)
                        .max(IPV6_HEADER_LEN)
                ];
                let (recv_size, src_addr) = self.try_recv(&mut tmp)?;
                let start = IPV6_HEADER_LEN.min(recv_size);
                let payload = &tmp[start..recv_size];
                let to_copy = core::cmp::min(payload.len(), user_buf.len());
                user_buf[..to_copy].copy_from_slice(&payload[..to_copy]);
                Ok((to_copy, src_addr))
            }
        }
    }

    /// 尝试发送数据包
    pub fn try_send(
        &self,
        buf: &[u8],
        to: Option<smoltcp::wire::IpAddress>,
    ) -> Result<usize, SystemError> {
        // Linux 语义：AF_INET6/SOCK_RAW/IPPROTO_RAW 可以创建，但写入返回 EINVAL。
        // gVisor raw_socket_test: RawSocketTest.IPv6ProtoRaw
        if self.is_ipv6() && self.protocol == IpProtocol::Unknown(255) {
            return Err(SystemError::EINVAL);
        }

        if let Some(dest) = to {
            if !self.addr_matches_ip_version(dest) {
                return Err(SystemError::EAFNOSUPPORT);
            }
        }

        // 确保已绑定
        if !self.is_bound() {
            if let Some(dest) = to {
                self.bind_ephemeral(dest)?;
            } else {
                return Err(SystemError::EDESTADDRREQ);
            }
        }

        let inner_guard = self.inner.read();
        match inner_guard.as_ref() {
            None => Err(SystemError::ENOTCONN),
            Some(RawInner::Bound(bound)) => {
                let sent = self.try_send_on_bound(bound, buf, to)?;
                bound.inner().iface().poll();
                Ok(sent)
            }
            Some(RawInner::Wildcard(bound)) => {
                let sent = self.try_send_on_bound(bound, buf, to)?;
                bound.inner().iface().poll();
                Ok(sent)
            }
            Some(RawInner::Unbound(_)) => Err(SystemError::ENOTCONN),
        }
    }

    fn try_send_on_bound(
        &self,
        bound: &inner::BoundRaw,
        buf: &[u8],
        to: Option<IpAddress>,
    ) -> Result<usize, SystemError> {
        let options = self.options.read().clone();

        // 目标地址：sendto() 显式指定优先；否则使用 connect(2) 的远端。
        let dest = to.or(bound.remote_addr());

        if options.ip_hdrincl {
            // 用户提供完整 IP 头，直接发送。
            return bound.try_send(buf, dest);
        }

        // 用户未提供 IP 头：按 Linux 语义内核自动构造。
        // gVisor raw_socket_test: RawSocketTest.ReceiveIPPacketInfo 等
        let dest = dest.ok_or(SystemError::EDESTADDRREQ)?;

        // 获取源地址
        let src = self.get_src_addr_for_send(bound, dest)?;

        let params = IpPacketParams {
            payload: buf,
            src,
            dst: dest,
            protocol: self.protocol,
            ttl: options.ip_ttl,
            tos: options.ip_tos,
            ipv6_checksum: options.ipv6_checksum,
        };

        let packet = build_ip_packet(self.ip_version, &params)?;

        // loopback 快速路径：避免 smoltcp 重序列化导致 TOS/TCLASS 丢失，
        // 并在此处实现 SO_RCVBUF 的投递/丢弃语义。
        if is_loopback_addr(dest) {
            let ctx = LoopbackDeliverContext {
                packet: &packet,
                dest,
                ip_version: self.ip_version,
                protocol: self.protocol,
                netns: &self.netns,
            };
            deliver_loopback_packet(&ctx);
            // Linux/Netstack：即便因 rcvbuf 满或过滤丢包，sendmsg/sendto 仍可成功。
            return Ok(buf.len());
        }

        bound.try_send(&packet, Some(dest))?;
        Ok(buf.len())
    }

    /// 获取发送时使用的源地址
    fn get_src_addr_for_send(
        &self,
        bound: &inner::BoundRaw,
        _dest: IpAddress,
    ) -> Result<IpAddress, SystemError> {
        match self.ip_version {
            IpVersion::Ipv4 => match bound.local_addr() {
                Some(addr @ IpAddress::Ipv4(_)) => Ok(addr),
                _ => {
                    let ip = bound
                        .inner()
                        .iface()
                        .common()
                        .ipv4_addr()
                        .ok_or(SystemError::EADDRNOTAVAIL)?;
                    let [a, b, c, d] = ip.octets();
                    Ok(IpAddress::Ipv4(Ipv4Address::new(a, b, c, d)))
                }
            },
            IpVersion::Ipv6 => match bound.local_addr() {
                Some(addr @ IpAddress::Ipv6(_)) => Ok(addr),
                _ => {
                    let iface = bound.inner().iface();
                    let addr = iface
                        .smol_iface()
                        .lock()
                        .ipv6_addr()
                        .ok_or(SystemError::EADDRNOTAVAIL)?;
                    Ok(IpAddress::Ipv6(addr))
                }
            },
        }
    }

    #[inline]
    pub fn can_recv(&self) -> bool {
        self.check_io_event().contains(EP::EPOLLIN)
    }

    #[inline]
    #[allow(dead_code)]
    pub fn can_send(&self) -> bool {
        self.check_io_event().contains(EP::EPOLLOUT)
    }

    #[allow(dead_code)]
    pub fn netns(&self) -> Arc<NetNamespace> {
        self.netns.clone()
    }

    /// 获取 IP 头长度
    fn get_ip_header_len(&self, data: &[u8]) -> usize {
        match self.ip_version {
            IpVersion::Ipv4 => {
                if data.is_empty() {
                    return IPV4_MIN_HEADER_LEN;
                }
                let ihl = (data[0] & 0x0F) as usize * 4;
                if ihl < IPV4_MIN_HEADER_LEN {
                    IPV4_MIN_HEADER_LEN
                } else {
                    ihl
                }
            }
            IpVersion::Ipv6 => IPV6_HEADER_LEN,
        }
    }

    /// 填充 peer 地址到 msg_name
    ///
    /// Linux 语义：peer port 固定为 0
    fn fill_peer_addr(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        src_addr: IpAddress,
    ) -> Result<(), SystemError> {
        if msg.msg_name.is_null() || msg.msg_namelen == 0 {
            return Ok(());
        }

        let port_be = 0u16.to_be();

        match (self.ip_version, src_addr) {
            (IpVersion::Ipv4, IpAddress::Ipv4(v4)) => {
                #[repr(C)]
                #[derive(Clone, Copy)]
                struct SockAddrIn {
                    sin_family: u16,
                    sin_port: u16,
                    sin_addr: u32,
                    sin_zero: [u8; 8],
                }
                let sa = SockAddrIn {
                    sin_family: crate::net::socket::AddressFamily::INet as u16,
                    sin_port: port_be,
                    sin_addr: u32::from_ne_bytes(v4.octets()),
                    sin_zero: [0; 8],
                };
                let want = core::mem::size_of::<SockAddrIn>().min(msg.msg_namelen as usize);
                let mut w = UserBufferWriter::new(msg.msg_name as *mut u8, want, true)?;
                let bytes = unsafe {
                    core::slice::from_raw_parts((&sa as *const SockAddrIn) as *const u8, want)
                };
                w.buffer_protected(0)?.write_to_user(0, bytes)?;
                msg.msg_namelen = want as u32;
            }
            (IpVersion::Ipv6, IpAddress::Ipv6(v6)) => {
                #[repr(C)]
                #[derive(Clone, Copy)]
                struct SockAddrIn6 {
                    sin6_family: u16,
                    sin6_port: u16,
                    sin6_flowinfo: u32,
                    sin6_addr: [u8; 16],
                    sin6_scope_id: u32,
                }
                let sa = SockAddrIn6 {
                    sin6_family: crate::net::socket::AddressFamily::INet6 as u16,
                    sin6_port: port_be,
                    sin6_flowinfo: 0,
                    sin6_addr: v6.octets(),
                    sin6_scope_id: 0,
                };
                let want = core::mem::size_of::<SockAddrIn6>().min(msg.msg_namelen as usize);
                let mut w = UserBufferWriter::new(msg.msg_name as *mut u8, want, true)?;
                let bytes = unsafe {
                    core::slice::from_raw_parts((&sa as *const SockAddrIn6) as *const u8, want)
                };
                w.buffer_protected(0)?.write_to_user(0, bytes)?;
                msg.msg_namelen = want as u32;
            }
            _ => {
                msg.msg_namelen = 0;
            }
        }
        Ok(())
    }

    /// 构建 IPv4 接收控制消息
    fn build_ipv4_cmsgs(
        &self,
        cmsg_buf: &mut CmsgBuffer,
        msg_flags: &mut i32,
        packet: &[u8],
        recv_size: usize,
        options: &RawSocketOptions,
    ) -> Result<(), SystemError> {
        if recv_size < IPV4_MIN_HEADER_LEN {
            return Ok(());
        }

        let tos = packet[1];
        let ttl = packet[8];
        let dst = u32::from_be_bytes([packet[16], packet[17], packet[18], packet[19]]);

        // IP_PKTINFO -> in_pktinfo
        if options.recv_pktinfo_v4 {
            let ifindex = self
                .inner
                .read()
                .as_ref()
                .and_then(|inner| match inner {
                    RawInner::Bound(b) | RawInner::Wildcard(b) => {
                        Some(b.inner().iface().nic_id() as i32)
                    }
                    _ => None,
                })
                .unwrap_or(0);
            let pktinfo = InPktInfo {
                ipi_ifindex: ifindex,
                ipi_spec_dst: dst.to_be(),
                ipi_addr: dst.to_be(),
            };
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    (&pktinfo as *const InPktInfo) as *const u8,
                    core::mem::size_of::<InPktInfo>(),
                )
            };
            cmsg_buf.put(
                msg_flags,
                PSOL::IP as i32,
                PIP::PKTINFO as i32,
                core::mem::size_of::<InPktInfo>(),
                bytes,
            )?;
        }

        // IP_RECVTOS
        if options.recv_tos {
            cmsg_buf.put(msg_flags, PSOL::IP as i32, PIP::TOS as i32, 1, &[tos])?;
        }

        // IP_RECVTTL
        if options.recv_ttl {
            let v = (ttl as i32).to_ne_bytes();
            cmsg_buf.put(
                msg_flags,
                PSOL::IP as i32,
                PIP::TTL as i32,
                core::mem::size_of::<i32>(),
                &v,
            )?;
        }

        Ok(())
    }

    /// 构建 IPv6 接收控制消息
    fn build_ipv6_cmsgs(
        &self,
        cmsg_buf: &mut CmsgBuffer,
        msg_flags: &mut i32,
        packet: &[u8],
        recv_size: usize,
        options: &RawSocketOptions,
    ) -> Result<(), SystemError> {
        if recv_size < IPV6_HEADER_LEN {
            return Ok(());
        }

        let traffic_class = ((packet[0] & 0x0f) << 4) | (packet[1] >> 4);
        let hop_limit = packet[7];
        let dst = &packet[24..40];

        // IPV6_RECVPKTINFO -> in6_pktinfo
        if options.recv_pktinfo_v6 {
            let ifindex = self
                .inner
                .read()
                .as_ref()
                .and_then(|inner| match inner {
                    RawInner::Bound(b) | RawInner::Wildcard(b) => {
                        Some(b.inner().iface().nic_id() as u32)
                    }
                    _ => None,
                })
                .unwrap_or(0);
            let mut pktinfo = In6PktInfo::default();
            pktinfo.ipi6_addr.copy_from_slice(dst);
            pktinfo.ipi6_ifindex = ifindex;
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    (&pktinfo as *const In6PktInfo) as *const u8,
                    core::mem::size_of::<In6PktInfo>(),
                )
            };
            cmsg_buf.put(
                msg_flags,
                PSOL::IPV6 as i32,
                PIPV6::PKTINFO as i32,
                core::mem::size_of::<In6PktInfo>(),
                bytes,
            )?;
        }

        // IPV6_RECVTCLASS
        if options.recv_tclass {
            let v = (traffic_class as i32).to_ne_bytes();
            cmsg_buf.put(
                msg_flags,
                PSOL::IPV6 as i32,
                PIPV6::TCLASS as i32,
                core::mem::size_of::<i32>(),
                &v,
            )?;
        }

        // IPV6_RECVHOPLIMIT
        if options.recv_hoplimit {
            let v = (hop_limit as i32).to_ne_bytes();
            cmsg_buf.put(
                msg_flags,
                PSOL::IPV6 as i32,
                PIPV6::HOPLIMIT as i32,
                core::mem::size_of::<i32>(),
                &v,
            )?;
        }

        Ok(())
    }

    /// 构建接收控制消息 (cmsg)
    ///
    /// 根据 socket 选项和接收的 IP 头信息，构建相应的控制消息
    fn build_recv_cmsgs(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        packet: &[u8],
        recv_size: usize,
    ) -> Result<usize, SystemError> {
        let mut write_off = 0usize;
        let mut cmsg_buf = CmsgBuffer {
            ptr: msg.msg_control,
            len: msg.msg_controllen,
            write_off: &mut write_off,
        };

        let options = self.options.read().clone();

        match self.ip_version {
            IpVersion::Ipv4 => self.build_ipv4_cmsgs(
                &mut cmsg_buf,
                &mut msg.msg_flags,
                packet,
                recv_size,
                &options,
            )?,
            IpVersion::Ipv6 => self.build_ipv6_cmsgs(
                &mut cmsg_buf,
                &mut msg.msg_flags,
                packet,
                recv_size,
                &options,
            )?,
        }

        Ok(write_off)
    }
}

impl Socket for RawSocket {
    fn open_file_counter(&self) -> &AtomicUsize {
        &self.open_files
    }

    fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }

    fn bind(&self, local_endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(endpoint) = local_endpoint {
            return self.do_bind(endpoint.addr);
        }
        Err(SystemError::EAFNOSUPPORT)
    }

    fn send_buffer_size(&self) -> usize {
        self.options.read().sock_sndbuf as usize
    }

    fn recv_buffer_size(&self) -> usize {
        self.options.read().sock_rcvbuf as usize
    }

    fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(remote) = endpoint {
            if !self.addr_matches_ip_version(remote.addr) {
                return Err(SystemError::EAFNOSUPPORT);
            }

            // Linux 语义：connect(2) 会为本端选择一个具体的可路由地址，
            // 使得 getsockname(2) 不返回 0.0.0.0/::。
            let need_local = match self.inner.read().as_ref() {
                Some(RawInner::Bound(b) | RawInner::Wildcard(b)) => b.local_addr().is_none(),
                _ => true,
            };

            // Linux 语义（对本测例）：connect(2) 后 getsockname 应返回可路由的本地地址。
            // 对 loopback 目标，直接绑定到 loopback 地址，避免返回 0.0.0.0/::。
            if need_local {
                let bind_local = match remote.addr {
                    IpAddress::Ipv4(v4) if v4.is_loopback() => Some(IpAddress::Ipv4(v4)),
                    IpAddress::Ipv6(v6) if v6.is_loopback() => Some(IpAddress::Ipv6(v6)),
                    _ => None,
                };
                if let Some(local) = bind_local {
                    self.do_bind(local)?;
                } else {
                    self.bind_ephemeral(remote.addr)?;
                }
            }
            let guard = self.inner.read();
            return match guard.as_ref() {
                Some(RawInner::Bound(inner)) => {
                    inner.connect(remote.addr);
                    Ok(())
                }
                Some(RawInner::Wildcard(inner)) => {
                    inner.connect(remote.addr);
                    Ok(())
                }
                _ => Err(SystemError::EINVAL),
            };
        }
        Err(SystemError::EAFNOSUPPORT)
    }

    fn validate_sendto_addr(
        &self,
        addr: *const crate::net::posix::SockAddr,
        addrlen: u32,
    ) -> Result<(), SystemError> {
        // Linux 语义：对 AF_INET6 socket，若用户提供目标地址但 addrlen 小于 sockaddr_in6，返回 EINVAL。
        // gVisor raw_socket_test: RawSocketTest.IPv6SendMsg
        if !addr.is_null()
            && self.is_ipv6()
            && (addrlen as usize) < core::mem::size_of::<crate::net::posix::SockAddrIn6>()
        {
            return Err(SystemError::EINVAL);
        }
        Ok(())
    }

    fn shutdown(&self, _how: crate::net::socket::common::ShutdownBit) -> Result<(), SystemError> {
        // Raw socket 的 shutdown 在 connect 前返回 ENOTCONN；connect 后为 no-op。
        let connected = match self.inner.read().as_ref() {
            Some(RawInner::Bound(b) | RawInner::Wildcard(b)) => b.remote_addr().is_some(),
            _ => false,
        };
        if connected {
            Ok(())
        } else {
            Err(SystemError::ENOTCONN)
        }
    }

    fn listen(&self, _backlog: usize) -> Result<(), SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn accept(&self) -> Result<(Arc<dyn Socket>, Endpoint), SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn send(&self, buffer: &[u8], flags: PMSG) -> Result<usize, SystemError> {
        if flags.contains(PMSG::DONTWAIT) || self.is_nonblock() {
            return self.try_send(buffer, None);
        }

        loop {
            match self.try_send(buffer, None) {
                Err(SystemError::ENOBUFS) => {
                    wq_wait_event_interruptible!(self.wait_queue, self.can_send(), {})?;
                }
                result => return result,
            }
        }
    }

    fn send_to(&self, buffer: &[u8], flags: PMSG, address: Endpoint) -> Result<usize, SystemError> {
        if let Endpoint::Ip(remote) = address {
            if flags.contains(PMSG::DONTWAIT) || self.is_nonblock() {
                return self.try_send(buffer, Some(remote.addr));
            }

            loop {
                match self.try_send(buffer, Some(remote.addr)) {
                    Err(SystemError::ENOBUFS) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.can_send(), {})?;
                    }
                    result => return result,
                }
            }
        }
        Err(SystemError::EINVAL)
    }

    fn recv(&self, buffer: &mut [u8], flags: PMSG) -> Result<usize, SystemError> {
        if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            self.try_recv_user(buffer).map(|(len, _)| len)
        } else {
            loop {
                match self.try_recv_user(buffer) {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
                    }
                    result => return result.map(|(len, _)| len),
                }
            }
        }
    }

    fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: PMSG,
        _address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        // Linux 语义：raw socket 的 recvfrom(2) 返回的 sockaddr_{in,in6}.port 为 0。
        let port = 0u16;
        if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            self.try_recv_user(buffer).map(|(len, addr)| {
                (
                    len,
                    Endpoint::Ip(smoltcp::wire::IpEndpoint::new(addr, port)),
                )
            })
        } else {
            loop {
                match self.try_recv_user(buffer) {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
                    }
                    result => {
                        return result.map(|(len, addr)| {
                            (
                                len,
                                Endpoint::Ip(smoltcp::wire::IpEndpoint::new(addr, port)),
                            )
                        })
                    }
                }
            }
        }
    }

    fn do_close(&self) -> Result<(), SystemError> {
        self.close();
        Ok(())
    }

    fn remote_endpoint(&self) -> Result<Endpoint, SystemError> {
        // Linux 语义：raw socket 的 getpeername(2) 即使 connect 之后也返回 ENOTCONN。
        Err(SystemError::ENOTCONN)
    }

    fn local_endpoint(&self) -> Result<Endpoint, SystemError> {
        let proto = self.protocol_u16();
        match self.inner.read().as_ref() {
            Some(RawInner::Bound(bound)) => {
                if let Some(addr) = bound.local_addr() {
                    Ok(Endpoint::Ip(smoltcp::wire::IpEndpoint::new(addr, proto)))
                } else {
                    // 返回未指定地址
                    match self.ip_version {
                        IpVersion::Ipv4 => Ok(Endpoint::Ip(smoltcp::wire::IpEndpoint::new(
                            IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED),
                            proto,
                        ))),
                        IpVersion::Ipv6 => Ok(Endpoint::Ip(smoltcp::wire::IpEndpoint::new(
                            IpAddress::Ipv6(smoltcp::wire::Ipv6Address::UNSPECIFIED),
                            proto,
                        ))),
                    }
                }
            }
            Some(RawInner::Wildcard(_)) | None => match self.ip_version {
                IpVersion::Ipv4 => Ok(Endpoint::Ip(smoltcp::wire::IpEndpoint::new(
                    IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED),
                    proto,
                ))),
                IpVersion::Ipv6 => Ok(Endpoint::Ip(smoltcp::wire::IpEndpoint::new(
                    IpAddress::Ipv6(smoltcp::wire::Ipv6Address::UNSPECIFIED),
                    proto,
                ))),
            },
            _ => match self.ip_version {
                IpVersion::Ipv4 => Ok(Endpoint::Ip(smoltcp::wire::IpEndpoint::new(
                    IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED),
                    proto,
                ))),
                IpVersion::Ipv6 => Ok(Endpoint::Ip(smoltcp::wire::IpEndpoint::new(
                    IpAddress::Ipv6(smoltcp::wire::Ipv6Address::UNSPECIFIED),
                    proto,
                ))),
            },
        }
    }

    fn recv_msg(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, true)? };

        // Linux 语义：IPv6 raw socket 接收时默认不向用户返回 IPv6 头；
        // 但控制消息(cmsg)需要从 IPv6 头提取，因此这里总是预留 IPv6 头空间。
        let user_len = iovs.total_len();
        let (need_head, extra_for_payload) = match self.ip_version {
            IpVersion::Ipv4 => (IPV4_MIN_HEADER_LEN, 0usize),
            IpVersion::Ipv6 => (IPV6_HEADER_LEN, IPV6_HEADER_LEN),
        };
        let mut tmp = vec![0u8; user_len.saturating_add(extra_for_payload).max(need_head)];

        let nonblock = self.is_nonblock() || flags.contains(PMSG::DONTWAIT);

        let (recv_size, src_addr) = if nonblock {
            self.try_recv(&mut tmp)
        } else {
            loop {
                match self.try_recv(&mut tmp) {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
                    }
                    other => break other,
                }
            }
        }?;

        // IPv4: 向用户返回整个 IPv4 包(含 IPv4 头)。
        // IPv6: 向用户仅返回 payload（不含 IPv6 固定头）。
        let (user_data, full_user_len) = match self.ip_version {
            IpVersion::Ipv4 => (&tmp[..recv_size], recv_size),
            IpVersion::Ipv6 => {
                let start = IPV6_HEADER_LEN.min(recv_size);
                let payload = &tmp[start..recv_size];
                (payload, payload.len())
            }
        };
        let user_recv_size = full_user_len.min(user_len);
        iovs.scatter(&user_data[..user_recv_size])?;

        // 默认不设置任何 flags。
        msg.msg_flags = 0;

        // 填充 peer 地址
        self.fill_peer_addr(msg, src_addr)?;

        // 构建控制消息
        msg.msg_controllen = self.build_recv_cmsgs(msg, &tmp, recv_size)?;

        Ok(user_recv_size)
    }

    fn send_msg(
        &self,
        msg: &crate::net::posix::MsgHdr,
        _flags: PMSG,
    ) -> Result<usize, SystemError> {
        // Gather payload.
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, false)? };
        let buf = iovs.gather()?;

        // Parse destination address if provided.
        let mut to_ip: Option<IpAddress> = if msg.msg_name.is_null() {
            None
        } else {
            let ep = SockAddr::to_endpoint(msg.msg_name as *const SockAddr, msg.msg_namelen)?;
            match ep {
                Endpoint::Ip(ip) => Some(ip.addr),
                _ => return Err(SystemError::EAFNOSUPPORT),
            }
        };

        // Clone current options and apply per-send overrides from cmsgs.
        let mut options = self.options.read().clone();

        if !msg.msg_control.is_null() && msg.msg_controllen != 0 {
            let reader =
                UserBufferReader::new(msg.msg_control as *const u8, msg.msg_controllen, true)?;
            let mut cbuf = vec![0u8; msg.msg_controllen];
            reader.copy_from_user(&mut cbuf, 0)?;

            let hdr_len = core::mem::size_of::<Cmsghdr>();
            let mut off = 0usize;

            let read_i32 = |d: &[u8]| -> Option<i32> {
                if d.len() >= 4 {
                    Some(i32::from_ne_bytes([d[0], d[1], d[2], d[3]]))
                } else {
                    None
                }
            };

            while off + hdr_len <= cbuf.len() {
                let hdr: Cmsghdr =
                    unsafe { core::ptr::read_unaligned(cbuf.as_ptr().add(off) as *const Cmsghdr) };
                if hdr.cmsg_len < hdr_len {
                    break;
                }

                let cmsg_len = core::cmp::min(hdr.cmsg_len, cbuf.len() - off);
                let data_off = off + cmsg_align(hdr_len);
                let data_len = cmsg_len.saturating_sub(cmsg_align(hdr_len));
                let data = if data_off <= cbuf.len() {
                    let end = core::cmp::min(data_off + data_len, cbuf.len());
                    &cbuf[data_off..end]
                } else {
                    &[]
                };

                match (hdr.cmsg_level, hdr.cmsg_type) {
                    (level, t) if level == PSOL::IP as i32 && t == PIP::TTL as i32 => {
                        if let Some(v) = read_i32(data) {
                            options.ip_ttl = v.clamp(0, 255) as u8;
                        }
                    }
                    (level, t) if level == PSOL::IP as i32 && t == PIP::TOS as i32 => {
                        // gVisor 的 SendTOS 使用 uint8_t 作为 cmsg value。
                        if let Some(&v) = data.first() {
                            options.ip_tos = v;
                        }
                    }
                    (level, t) if level == PSOL::IPV6 as i32 && t == PIPV6::HOPLIMIT as i32 => {
                        if let Some(v) = read_i32(data) {
                            options.ip_ttl = v.clamp(0, 255) as u8;
                        }
                    }
                    (level, t) if level == PSOL::IPV6 as i32 && t == PIPV6::TCLASS as i32 => {
                        if let Some(v) = read_i32(data) {
                            options.ip_tos = v.clamp(0, 255) as u8;
                        }
                    }
                    _ => {}
                }

                let step = cmsg_align(cmsg_len);
                if step == 0 {
                    break;
                }
                off = off.saturating_add(step);
            }
        }

        // Resolve destination from connect(2) if not explicitly provided.
        if to_ip.is_none() {
            if let Some(RawInner::Bound(b) | RawInner::Wildcard(b)) = self.inner.read().as_ref() {
                to_ip = b.remote_addr();
            }
        }
        let dest = to_ip.ok_or(SystemError::EDESTADDRREQ)?;

        // Ensure bound.
        if !self.is_bound() {
            self.bind_ephemeral(dest)?;
        }

        let inner_guard = self.inner.read();
        let bound = match inner_guard.as_ref() {
            Some(RawInner::Bound(b)) => b,
            Some(RawInner::Wildcard(b)) => b,
            _ => return Err(SystemError::ENOTCONN),
        };

        if options.ip_hdrincl {
            bound.try_send(&buf, Some(dest))?;
            bound.inner().iface().poll();
            return Ok(buf.len());
        }

        // 获取源地址
        let src = self.get_src_addr_for_send(bound, dest)?;

        let params = IpPacketParams {
            payload: &buf,
            src,
            dst: dest,
            protocol: self.protocol,
            ttl: options.ip_ttl,
            tos: options.ip_tos,
            ipv6_checksum: options.ipv6_checksum,
        };

        let packet = build_ip_packet(self.ip_version, &params)?;

        // loopback 快速路径
        if is_loopback_addr(dest) {
            let ctx = LoopbackDeliverContext {
                packet: &packet,
                dest,
                ip_version: self.ip_version,
                protocol: self.protocol,
                netns: &self.netns,
            };
            deliver_loopback_packet(&ctx);
            // Linux/Netstack：即便因 rcvbuf 满或过滤丢包，sendmsg 仍可成功。
            return Ok(buf.len());
        }

        bound.try_send(&packet, Some(dest))?;
        bound.inner().iface().poll();
        Ok(buf.len())
    }

    fn epoll_items(&self) -> &EPollItems {
        &self.epoll_items
    }

    fn fasync_items(&self) -> &FAsyncItems {
        &self.fasync_items
    }

    fn check_io_event(&self) -> EPollEventType {
        let mut event = EPollEventType::empty();

        if !self.loopback_rx.lock_irqsave().pkts.is_empty() {
            event.insert(EP::EPOLLIN | EP::EPOLLRDNORM);
        }

        match self.inner.read().as_ref() {
            None | Some(RawInner::Unbound(_)) => {
                event.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
            }
            Some(RawInner::Wildcard(bound)) => {
                let (can_recv, can_send) =
                    bound.with_socket(|socket| (socket.can_recv(), socket.can_send()));

                if can_recv {
                    event.insert(EP::EPOLLIN | EP::EPOLLRDNORM);
                }

                if can_send {
                    event.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
                }
            }
            Some(RawInner::Bound(bound)) => {
                let (can_recv, can_send) =
                    bound.with_socket(|socket| (socket.can_recv(), socket.can_send()));

                if can_recv {
                    event.insert(EP::EPOLLIN | EP::EPOLLRDNORM);
                }

                if can_send {
                    event.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
                }
            }
        }
        event
    }

    fn socket_inode_id(&self) -> InodeId {
        self.inode_id
    }

    fn option(&self, level: PSOL, name: usize, value: &mut [u8]) -> Result<usize, SystemError> {
        match level {
            PSOL::SOCKET => match PSO::try_from(name as u32) {
                Ok(PSO::SNDBUF) => {
                    if value.len() < 4 {
                        return Err(SystemError::EINVAL);
                    }
                    let v = self.options.read().sock_sndbuf;
                    value[..4].copy_from_slice(&v.to_ne_bytes());
                    Ok(4)
                }
                Ok(PSO::RCVBUF) => {
                    if value.len() < 4 {
                        return Err(SystemError::EINVAL);
                    }
                    let v = self.options.read().sock_rcvbuf;
                    value[..4].copy_from_slice(&v.to_ne_bytes());
                    Ok(4)
                }
                Ok(PSO::BINDTODEVICE) => {
                    let name = self
                        .options
                        .read()
                        .bind_to_device
                        .clone()
                        .unwrap_or_default();
                    let need = core::cmp::min(name.len() + 1, IFNAMSIZ);
                    if value.len() < need {
                        return Err(SystemError::EINVAL);
                    }
                    if need == 0 {
                        return Ok(0);
                    }
                    let bytes = name.as_bytes();
                    let copy_len = core::cmp::min(bytes.len(), need.saturating_sub(1));
                    value[..copy_len].copy_from_slice(&bytes[..copy_len]);
                    value[copy_len] = 0;
                    Ok(need)
                }
                Ok(PSO::DETACH_FILTER) => Err(SystemError::ENOPROTOOPT),
                _ => Err(SystemError::ENOPROTOOPT),
            },
            PSOL::RAW => match PRAW::try_from(name as u32) {
                Ok(PRAW::ICMP_FILTER) => {
                    if self.protocol != IpProtocol::Icmp {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    if value.len() < 4 {
                        return Err(SystemError::EINVAL);
                    }
                    let mask = self.options.read().icmp_filter.get_mask();
                    value[..4].copy_from_slice(&mask.to_ne_bytes());
                    Ok(4)
                }
                _ => Err(SystemError::ENOPROTOOPT),
            },
            PSOL::IP => match PIP::try_from(name as u32) {
                Ok(PIP::HDRINCL) => {
                    let v = if self.options.read().ip_hdrincl {
                        1i32
                    } else {
                        0i32
                    };
                    let len = core::cmp::min(value.len(), 4);
                    value[..len].copy_from_slice(&v.to_ne_bytes()[..len]);
                    Ok(len)
                }
                Ok(PIP::TOS) => {
                    if value.len() < 4 {
                        return Err(SystemError::EINVAL);
                    }
                    let v = self.options.read().ip_tos as i32;
                    value[..4].copy_from_slice(&v.to_ne_bytes());
                    Ok(4)
                }
                Ok(PIP::TTL) => {
                    if value.len() < 4 {
                        return Err(SystemError::EINVAL);
                    }
                    let v = self.options.read().ip_ttl as i32;
                    value[..4].copy_from_slice(&v.to_ne_bytes());
                    Ok(4)
                }
                Ok(PIP::PKTINFO) => {
                    let v = if self.options.read().recv_pktinfo_v4 {
                        1i32
                    } else {
                        0i32
                    };
                    let len = core::cmp::min(value.len(), 4);
                    value[..len].copy_from_slice(&v.to_ne_bytes()[..len]);
                    Ok(len)
                }
                Ok(PIP::RECVTTL) => {
                    let v = if self.options.read().recv_ttl {
                        1i32
                    } else {
                        0i32
                    };
                    let len = core::cmp::min(value.len(), 4);
                    value[..len].copy_from_slice(&v.to_ne_bytes()[..len]);
                    Ok(len)
                }
                Ok(PIP::RECVTOS) => {
                    let v = if self.options.read().recv_tos {
                        1i32
                    } else {
                        0i32
                    };
                    let len = core::cmp::min(value.len(), 4);
                    value[..len].copy_from_slice(&v.to_ne_bytes()[..len]);
                    Ok(len)
                }
                _ => Err(SystemError::ENOPROTOOPT),
            },
            PSOL::IPV6 => {
                match PIPV6::try_from(name as u32) {
                    // IPV6_CHECKSUM = 7
                    Ok(PIPV6::CHECKSUM) => {
                        if value.len() < 4 {
                            return Err(SystemError::EINVAL);
                        }
                        let v = self.options.read().ipv6_checksum;
                        value[..4].copy_from_slice(&v.to_ne_bytes());
                        Ok(4)
                    }
                    // IPV6_UNICAST_HOPS = 16
                    Ok(PIPV6::UNICAST_HOPS) => {
                        if value.len() < 4 {
                            return Err(SystemError::EINVAL);
                        }
                        let v = self.options.read().ip_ttl as i32;
                        value[..4].copy_from_slice(&v.to_ne_bytes());
                        Ok(4)
                    }
                    // IPV6_TCLASS = 67
                    Ok(PIPV6::TCLASS) => {
                        if value.len() < 4 {
                            return Err(SystemError::EINVAL);
                        }
                        let v = self.options.read().ip_tos as i32;
                        value[..4].copy_from_slice(&v.to_ne_bytes());
                        Ok(4)
                    }
                    // IPV6_RECVPKTINFO = 49
                    Ok(PIPV6::RECVPKTINFO) => {
                        let v = if self.options.read().recv_pktinfo_v6 {
                            1i32
                        } else {
                            0i32
                        };
                        let len = core::cmp::min(value.len(), 4);
                        value[..len].copy_from_slice(&v.to_ne_bytes()[..len]);
                        Ok(len)
                    }
                    // IPV6_RECVHOPLIMIT = 51
                    Ok(PIPV6::RECVHOPLIMIT) => {
                        let v = if self.options.read().recv_hoplimit {
                            1i32
                        } else {
                            0i32
                        };
                        let len = core::cmp::min(value.len(), 4);
                        value[..len].copy_from_slice(&v.to_ne_bytes()[..len]);
                        Ok(len)
                    }
                    // IPV6_RECVTCLASS = 66
                    Ok(PIPV6::RECVTCLASS) => {
                        let v = if self.options.read().recv_tclass {
                            1i32
                        } else {
                            0i32
                        };
                        let len = core::cmp::min(value.len(), 4);
                        value[..len].copy_from_slice(&v.to_ne_bytes()[..len]);
                        Ok(len)
                    }
                    _ => Err(SystemError::ENOPROTOOPT),
                }
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn set_option(&self, level: PSOL, name: usize, val: &[u8]) -> Result<(), SystemError> {
        match level {
            PSOL::SOCKET => match PSO::try_from(name as u32) {
                Ok(PSO::SNDBUF) => {
                    let v = sock_buf_u32_from_opt(val)?;
                    let newv = clamp_sock_buf(v, SYSCTL_WMEM_MAX, SOCK_MIN_SNDBUF);
                    self.options.write().sock_sndbuf = newv;
                    Ok(())
                }
                Ok(PSO::RCVBUF) => {
                    let v = sock_buf_u32_from_opt(val)?;
                    let newv = clamp_sock_buf(v, SYSCTL_RMEM_MAX, SOCK_MIN_RCVBUF);
                    self.options.write().sock_rcvbuf = newv;
                    Ok(())
                }
                Ok(PSO::BINDTODEVICE) => {
                    // Linux: optval 为 char[]，空字符串表示解绑。
                    let end = val.iter().position(|&b| b == 0).unwrap_or(val.len());
                    let name_bytes = &val[..end];
                    if name_bytes.is_empty() {
                        self.options.write().bind_to_device = None;
                        return Ok(());
                    }
                    let name = core::str::from_utf8(name_bytes).map_err(|_| SystemError::EINVAL)?;
                    // 校验设备存在。
                    let found = self
                        .netns
                        .device_list()
                        .values()
                        .any(|iface| iface.iface_name() == name);
                    if !found {
                        return Err(SystemError::ENODEV);
                    }
                    self.options.write().bind_to_device = Some(String::from(name));
                    Ok(())
                }
                Ok(PSO::DETACH_FILTER) => {
                    // Linux: 未安装 filter 时返回 ENOENT。
                    let mut opts = self.options.write();
                    if !opts.filter_attached {
                        return Err(SystemError::ENOENT);
                    }
                    opts.filter_attached = false;
                    Ok(())
                }
                _ => Ok(()),
            },
            PSOL::RAW => match PRAW::try_from(name as u32) {
                Ok(PRAW::ICMP_FILTER) => {
                    if self.protocol != IpProtocol::Icmp {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    if val.len() < 4 {
                        return Err(SystemError::EINVAL);
                    }
                    let mask = u32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
                    self.options.write().icmp_filter.set_mask(mask);
                    Ok(())
                }
                _ => Err(SystemError::ENOPROTOOPT),
            },
            PSOL::IP => {
                match PIP::try_from(name as u32) {
                    Ok(PIP::HDRINCL) => {
                        let enable = val.first().copied().unwrap_or(0) != 0;
                        self.options.write().ip_hdrincl = enable;
                        Ok(())
                    }
                    Ok(PIP::TOS) => {
                        let v =
                            read_i32_opt(val).unwrap_or(val.first().copied().unwrap_or(0) as i32);
                        if !(0..=255).contains(&v) {
                            return Err(SystemError::EINVAL);
                        }
                        self.options.write().ip_tos = v as u8;
                        Ok(())
                    }
                    Ok(PIP::TTL) => {
                        let v = read_i32_opt(val)
                            .unwrap_or(val.first().copied().unwrap_or(DEFAULT_IP_TTL) as i32);
                        if !(0..=255).contains(&v) {
                            return Err(SystemError::EINVAL);
                        }
                        self.options.write().ip_ttl = v as u8;
                        Ok(())
                    }
                    Ok(PIP::PKTINFO) => {
                        let enable = val.first().copied().unwrap_or(0) != 0;
                        self.options.write().recv_pktinfo_v4 = enable;
                        Ok(())
                    }
                    Ok(PIP::RECVTTL) => {
                        let enable = val.first().copied().unwrap_or(0) != 0;
                        self.options.write().recv_ttl = enable;
                        Ok(())
                    }
                    Ok(PIP::RECVTOS) => {
                        let enable = val.first().copied().unwrap_or(0) != 0;
                        self.options.write().recv_tos = enable;
                        Ok(())
                    }
                    _ => Ok(()), // 保持既有策略：忽略未实现的选项
                }
            }
            PSOL::IPV6 => {
                match PIPV6::try_from(name as u32) {
                    Ok(PIPV6::CHECKSUM) => {
                        let v = read_i32_opt(val).ok_or(SystemError::EINVAL)?;
                        if v != -1 {
                            if v < 0 {
                                return Err(SystemError::EINVAL);
                            }
                            // Linux: 偏移必须为 2 字节对齐。
                            if (v & 1) != 0 {
                                return Err(SystemError::EINVAL);
                            }
                        }
                        self.options.write().ipv6_checksum = v;
                        Ok(())
                    }
                    Ok(PIPV6::UNICAST_HOPS) => {
                        let v = read_i32_opt(val).ok_or(SystemError::EINVAL)?;
                        if v == -1 {
                            // -1 表示系统默认值，这里保持当前默认(64)不变。
                            return Ok(());
                        }
                        if !(0..=255).contains(&v) {
                            return Err(SystemError::EINVAL);
                        }
                        self.options.write().ip_ttl = v as u8;
                        Ok(())
                    }
                    Ok(PIPV6::TCLASS) => {
                        let v = read_i32_opt(val).ok_or(SystemError::EINVAL)?;
                        if !(0..=255).contains(&v) {
                            return Err(SystemError::EINVAL);
                        }
                        self.options.write().ip_tos = v as u8;
                        Ok(())
                    }
                    Ok(PIPV6::RECVPKTINFO) => {
                        let enable = val.first().copied().unwrap_or(0) != 0;
                        self.options.write().recv_pktinfo_v6 = enable;
                        Ok(())
                    }
                    Ok(PIPV6::RECVHOPLIMIT) => {
                        let enable = val.first().copied().unwrap_or(0) != 0;
                        self.options.write().recv_hoplimit = enable;
                        Ok(())
                    }
                    Ok(PIPV6::RECVTCLASS) => {
                        let enable = val.first().copied().unwrap_or(0) != 0;
                        self.options.write().recv_tclass = enable;
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }
            _ => Ok(()),
        }
    }
}

impl InetSocket for RawSocket {
    fn on_iface_events(&self) {
        // Raw socket 不需要特殊的接口事件处理
    }
}
