use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use smoltcp::wire::{IpAddress, IpProtocol, IpVersion};

use crate::libs::rwlock::RwLock;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::namespace::NamespaceOps;
use crate::process::ProcessState;

use super::constants::{
    ICMPV6_CHECKSUM_OFFSET, ICMPV6_ECHO_REPLY, ICMPV6_ECHO_REQUEST, ICMP_ECHO_REPLY,
    ICMP_ECHO_REQUEST,
};
use super::inner::RawInner;
use super::options::DEFAULT_IP_TTL;
use super::packet::{
    build_ip_packet, checksum16, ipv6_icmpv6_checksum, ipv6_udp_checksum, IpPacketParams,
};
use super::{InetSocket, RawSocket};
use crate::net::socket::utils::{IPV4_MIN_HEADER_LEN, IPV6_HEADER_LEN};

// SKB 内存计费常量 (参考 Linux 6.6 include/linux/skbuff.h)
/// SKB 数据对齐大小 (SMP_CACHE_BYTES)
const SKB_DATA_ALIGN: usize = 64;
/// SKB 管理开销 (sizeof(sk_buff) + sizeof(skb_shared_info) 对齐后)
const SKB_OVERHEAD: usize = 576;

#[derive(Debug, Default)]
pub(super) struct LoopbackRxQueue {
    pub(super) pkts: VecDeque<Vec<u8>>,
    pub(super) bytes: usize,
}

lazy_static! {
    /// 同一 netns 下的 raw socket 列表（用于 loopback 快速路径广播投递）。
    static ref RAW_SOCKET_REGISTRY: RwLock<BTreeMap<usize, Vec<Weak<RawSocket>>>> =
        RwLock::new(BTreeMap::new());
}

pub(super) fn register_raw_socket(sock: &Arc<RawSocket>) {
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

/// 检查目标地址是否为 loopback
#[inline]
pub(super) fn is_loopback_addr(addr: IpAddress) -> bool {
    match addr {
        IpAddress::Ipv4(v4) => v4.is_loopback(),
        IpAddress::Ipv6(v6) => v6.is_loopback(),
    }
}

/// Loopback 投递上下文
pub(super) struct LoopbackDeliverContext<'a> {
    pub(super) packet: &'a [u8],
    pub(super) dest: IpAddress,
    pub(super) ip_version: IpVersion,
    pub(super) protocol: IpProtocol,
    pub(super) netns: &'a Arc<NetNamespace>,
}

/// 检查 socket 是否应该接收该 loopback 数据包
///
/// 应用以下过滤规则：
/// 1. IP 版本和协议号匹配
/// 2. ICMP/ICMPv6 filter
/// 3. SO_BINDTODEVICE (loopback 视为 "lo")
/// 4. bind(2) 目的地址过滤
/// 5. IPV6_CHECKSUM 接收校验
fn should_deliver_to_socket(s: &RawSocket, ctx: &LoopbackDeliverContext) -> bool {
    // 1. IP 版本和协议号必须匹配
    if s.ip_version != ctx.ip_version || s.protocol != ctx.protocol {
        return false;
    }

    // 2. ICMP filter
    if ctx.protocol == IpProtocol::Icmp {
        let ihl = if ctx.packet.len() >= IPV4_MIN_HEADER_LEN {
            ((ctx.packet[0] & 0x0f) as usize) * 4
        } else {
            0
        };
        if ihl >= IPV4_MIN_HEADER_LEN && ctx.packet.len() > ihl {
            let icmp_type = ctx.packet[ihl];
            if s.options.read().icmp_filter.should_filter(icmp_type) {
                return false;
            }
        }
    }

    // 3. ICMPv6 filter
    if ctx.protocol == IpProtocol::Icmpv6 && ctx.packet.len() > IPV6_HEADER_LEN {
        let icmp_type = ctx.packet[IPV6_HEADER_LEN];
        if s.options.read().icmp6_filter.should_filter(icmp_type) {
            return false;
        }
    }

    // 4. SO_BINDTODEVICE：loopback 快速路径视为来自 lo
    if let Some(dev) = &s.options.read().bind_to_device {
        if dev.as_str() != "lo" {
            return false;
        }
    }

    // 5. bind(2) 目的地址过滤
    let local = match s.inner.read().as_ref() {
        Some(RawInner::Bound(b) | RawInner::Wildcard(b)) => b.local_addr(),
        _ => None,
    };
    if let Some(local) = local {
        if local != ctx.dest {
            return false;
        }
    }

    // 6. IPV6_CHECKSUM 接收校验
    if s.ip_version == IpVersion::Ipv6 && s.protocol == IpProtocol::Udp {
        let off = s.options.read().ipv6_checksum;
        if off >= 0 {
            let off = off as usize;
            if ctx.packet.len() < IPV6_HEADER_LEN {
                return false;
            }
            let payload = &ctx.packet[IPV6_HEADER_LEN..];
            if off + 2 > payload.len() {
                return false;
            }
            let got = u16::from_be_bytes([payload[off], payload[off + 1]]);
            if got == 0 {
                return false;
            }
            match ipv6_udp_checksum(ctx.packet, off) {
                Some(expect) if expect == got => {}
                _ => return false,
            }
        }
    }

    true
}

/// 向同一 netns 下所有匹配的 raw socket 投递 loopback 数据包
pub(super) fn deliver_loopback_packet(ctx: &LoopbackDeliverContext) {
    let sockets = raw_sockets_in_netns(ctx.netns);
    let pkt_cost = loopback_rx_mem_cost(ctx.packet.len());

    for s in sockets.iter() {
        if !should_deliver_to_socket(s, ctx) {
            continue;
        }

        // SO_RCVBUF：投递/丢弃语义
        let rcvbuf = s.options.read().sock_rcvbuf as usize;
        let enqueued = {
            let mut q = s.loopback_rx.lock_irqsave();
            let can_enqueue = if q.bytes == 0 {
                // Linux/Netstack：当接收队列为空时，允许接收一个超过 rcvbuf 的 dgram
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

    // Linux 语义：本机收到 ICMP/ICMPv6 Echo Request 时自动回复 Echo Reply
    handle_loopback_echo_request(ctx);
}

/// Echo Reply 构建参数
struct EchoReplyParams {
    reply_payload: Vec<u8>,
    /// 原始目的地址，作为 reply 的源
    src: IpAddress,
    /// 原始源地址，作为 reply 的目的
    dst: IpAddress,
}

/// 尝试从 IPv4 ICMP 包提取 Echo Request 并构建 Reply 参数
fn try_build_icmp_echo_reply(packet: &[u8]) -> Option<EchoReplyParams> {
    if packet.len() < IPV4_MIN_HEADER_LEN {
        return None;
    }
    let ihl = ((packet[0] & 0x0f) as usize) * 4;
    if ihl < IPV4_MIN_HEADER_LEN || packet.len() < ihl + 8 {
        return None;
    }
    let payload = &packet[ihl..];

    // 检查是否为 Echo Request (type=8, code=0)
    if payload[0] != ICMP_ECHO_REQUEST || payload[1] != 0 {
        return None;
    }
    // 校验和正确时才回复（BadChecksum 测例要求没有 reply）
    if checksum16(payload) != 0xffff {
        return None;
    }
    if packet.len() < 20 {
        return None;
    }

    let src = smoltcp::wire::Ipv4Address::new(packet[12], packet[13], packet[14], packet[15]);
    let dst = smoltcp::wire::Ipv4Address::new(packet[16], packet[17], packet[18], packet[19]);

    // 构建 reply payload
    let mut reply_payload = payload.to_vec();
    reply_payload[0] = ICMP_ECHO_REPLY;
    reply_payload[2] = 0;
    reply_payload[3] = 0;
    let xsum = checksum16(&reply_payload);
    reply_payload[2..4].copy_from_slice(&xsum.to_be_bytes());

    Some(EchoReplyParams {
        reply_payload,
        src: IpAddress::Ipv4(dst),
        dst: IpAddress::Ipv4(src),
    })
}

/// 尝试从 IPv6 ICMPv6 包提取 Echo Request 并构建 Reply 参数
fn try_build_icmpv6_echo_reply(packet: &[u8]) -> Option<EchoReplyParams> {
    if packet.len() < IPV6_HEADER_LEN + 4 {
        return None;
    }
    let payload = &packet[IPV6_HEADER_LEN..];

    // 检查是否为 Echo Request (type=128, code=0)
    if payload[0] != ICMPV6_ECHO_REQUEST || payload[1] != 0 {
        return None;
    }
    // 校验和验证
    let got = u16::from_be_bytes([payload[2], payload[3]]);
    if got == 0 {
        return None;
    }
    match ipv6_icmpv6_checksum(packet, ICMPV6_CHECKSUM_OFFSET as usize) {
        Some(expect) if expect == got => {}
        _ => return None,
    }
    if packet.len() < 40 {
        return None;
    }

    let src_bytes: [u8; 16] = packet[8..24].try_into().ok()?;
    let dst_bytes: [u8; 16] = packet[24..40].try_into().ok()?;
    let src = smoltcp::wire::Ipv6Address::new(
        u16::from_be_bytes([src_bytes[0], src_bytes[1]]),
        u16::from_be_bytes([src_bytes[2], src_bytes[3]]),
        u16::from_be_bytes([src_bytes[4], src_bytes[5]]),
        u16::from_be_bytes([src_bytes[6], src_bytes[7]]),
        u16::from_be_bytes([src_bytes[8], src_bytes[9]]),
        u16::from_be_bytes([src_bytes[10], src_bytes[11]]),
        u16::from_be_bytes([src_bytes[12], src_bytes[13]]),
        u16::from_be_bytes([src_bytes[14], src_bytes[15]]),
    );
    let dst = smoltcp::wire::Ipv6Address::new(
        u16::from_be_bytes([dst_bytes[0], dst_bytes[1]]),
        u16::from_be_bytes([dst_bytes[2], dst_bytes[3]]),
        u16::from_be_bytes([dst_bytes[4], dst_bytes[5]]),
        u16::from_be_bytes([dst_bytes[6], dst_bytes[7]]),
        u16::from_be_bytes([dst_bytes[8], dst_bytes[9]]),
        u16::from_be_bytes([dst_bytes[10], dst_bytes[11]]),
        u16::from_be_bytes([dst_bytes[12], dst_bytes[13]]),
        u16::from_be_bytes([dst_bytes[14], dst_bytes[15]]),
    );

    // 构建 reply payload (校验和由 build_ipv6_packet 计算)
    let mut reply_payload = payload.to_vec();
    reply_payload[0] = ICMPV6_ECHO_REPLY;
    reply_payload[2] = 0;
    reply_payload[3] = 0;

    Some(EchoReplyParams {
        reply_payload,
        src: IpAddress::Ipv6(dst),
        dst: IpAddress::Ipv6(src),
    })
}

/// 处理 loopback Echo Request，生成并投递 Echo Reply
fn handle_loopback_echo_request(ctx: &LoopbackDeliverContext) {
    let reply_params = match (ctx.ip_version, ctx.protocol) {
        (IpVersion::Ipv4, IpProtocol::Icmp) => try_build_icmp_echo_reply(ctx.packet),
        (IpVersion::Ipv6, IpProtocol::Icmpv6) => try_build_icmpv6_echo_reply(ctx.packet),
        _ => return,
    };

    let Some(params) = reply_params else { return };

    let ip_params = IpPacketParams {
        payload: &params.reply_payload,
        src: params.src,
        dst: params.dst,
        protocol: ctx.protocol,
        ttl: DEFAULT_IP_TTL,
        tos: 0,
        ipv6_checksum: if ctx.ip_version == IpVersion::Ipv6 {
            ICMPV6_CHECKSUM_OFFSET
        } else {
            -1
        },
    };

    if let Ok(reply_packet) = build_ip_packet(ctx.ip_version, &ip_params) {
        let reply_ctx = LoopbackDeliverContext {
            packet: &reply_packet,
            dest: params.dst,
            ip_version: ctx.ip_version,
            protocol: ctx.protocol,
            netns: ctx.netns,
        };
        deliver_loopback_packet(&reply_ctx);
    }
}

#[inline]
pub(super) fn loopback_rx_mem_cost(pkt_len: usize) -> usize {
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
