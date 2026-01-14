//! TCP listen/backlog 语义辅助：在不修改 smoltcp 的前提下尽量贴近 Linux 行为。
//!
//! 背景：
//! - DragonOS 的 inet stream(TCP) 通过“多个 smoltcp LISTEN socket”来模拟 accept 队列槽位；
//! - smoltcp 在“没有 socket 可以处理该端口”的情况下，可能会发送 RST（尤其是 lo/IPv6 同一轮 poll 内，
//!   第一个 SYN 消耗掉 LISTEN socket，第二个 SYN 立即触发 RST）。
//! - Linux 6.6 在 accept 队列满时通常是静默丢 SYN（参考 `tcp_conn_request()->goto drop`），
//!   backlog==0 时因为 `sk_acceptq_is_full()` 使用 `>`，仍会允许 1 个 pending。
//!
//! 因此我们需要一个“在进入 smoltcp 前可选丢包”的策略组件，但它必须：
//! - 不在设备收包路径里再次加锁 `SocketSet`（避免 poll 持锁时死锁）；
//! - 能在 `IfaceCommon::poll()` 持有 SocketSet 锁时刷新缓存；
//! - 仅对 backlog==0 这种需要 Linux-like timeout 行为的端口启用（当前策略）。

use alloc::vec::Vec;

use crate::libs::rwsem::RwSem;
use smoltcp::wire::{
    EthernetFrame, EthernetProtocol, IpProtocol, Ipv4Packet, Ipv6Packet, TcpPacket,
};

#[derive(Debug, Clone, Copy)]
struct TcpListenPortInfo {
    port: u16,
    /// backlog==0 时启用：当"本轮 poll 里 LISTEN socket 被消耗完"后，后续纯 SYN 直接丢弃（不让 smoltcp 回 RST）。
    drop_syn_when_full: bool,
    /// 缓存：当前是否存在至少一个处于 LISTEN 状态的 smoltcp socket（同端口）。
    ///
    /// 注意：必须在 `IfaceCommon::poll()` 持有 SocketSet 锁时刷新，且丢包判断路径不得再锁 SocketSet。
    listen_socket_present: bool,
}

/// 每个 Iface 维护一份 listener/backlog 状态，用于收包入口的"是否丢 SYN"决策。
#[derive(Debug)]
pub struct TcpListenerBacklog {
    ports: RwSem<Vec<TcpListenPortInfo>>,
}

impl TcpListenerBacklog {
    pub fn new() -> Self {
        Self {
            ports: RwSem::new(Vec::new()),
        }
    }

    pub fn register_tcp_listen_port(&self, port: u16, backlog: usize) {
        let mut guard = self.ports.write();
        // gVisor 期望 listen(backlog=0) 只允许 1 个 pending，额外 SYN 应 timeout（丢包）。
        // backlog>0 时维持 smoltcp 默认行为（当前策略），避免把本应尽快暴露的错误（RST）变成超时。
        let drop_syn_when_full = backlog == 0;
        if let Some(e) = guard.iter_mut().find(|e| e.port == port) {
            e.drop_syn_when_full = drop_syn_when_full;
            // 保守：假设 present，等待下一次 poll 刷新。
            e.listen_socket_present = true;
        } else {
            guard.push(TcpListenPortInfo {
                port,
                drop_syn_when_full,
                listen_socket_present: true,
            });
        }
    }

    pub fn unregister_tcp_listen_port(&self, port: u16) {
        let mut guard = self.ports.write();
        if let Some(i) = guard.iter().position(|e| e.port == port) {
            guard.swap_remove(i);
        }
    }

    /// 在持有 smoltcp SocketSet 锁的前提下刷新缓存。
    ///
    /// IMPORTANT: 不要在这里做分配/collect，避免与全局分配器锁产生复杂死锁。
    pub fn refresh_listen_socket_present(&self, sockets: &smoltcp::iface::SocketSet<'static>) {
        let mut guard = self.ports.write();
        for entry in guard.iter_mut() {
            let mut present = false;
            for item in sockets.items() {
                if let smoltcp::socket::Socket::Tcp(tcp) = &item.socket {
                    if tcp.state() == smoltcp::socket::tcp::State::Listen
                        && tcp.listen_endpoint().port == entry.port
                    {
                        present = true;
                        break;
                    }
                }
            }
            entry.listen_socket_present = present;
        }
    }

    #[inline]
    fn any_drop_syn_ports(&self) -> bool {
        self.ports.read().iter().any(|e| e.drop_syn_when_full)
    }

    /// 判定是否应当丢弃一个“纯 SYN”TCP 包（用于 backlog==0 的 Linux-like 行为）。
    ///
    /// - 该函数 **不得** 访问/加锁 SocketSet；
    /// - 仅当目的端口注册为 backlog==0 且本轮 poll 已经“消耗掉 LISTEN socket”时，才会丢弃。
    pub fn should_drop_backlog_full_tcp_syn_ip(&self, ip_packet: &[u8]) -> bool {
        let mut dst_port: Option<u16> = None;
        let mut is_pure_syn = false;

        if !self.any_drop_syn_ports() {
            return false;
        }

        // 有些路径可能给原始 IP（Medium::Ip）或以太网帧（Medium::Ethernet）。
        // 先尝试直接按 IP 解析，失败再尝试 Ethernet->IP。
        let mut maybe_ip: &[u8] = ip_packet;
        if Ipv4Packet::new_checked(maybe_ip).is_err() && Ipv6Packet::new_checked(maybe_ip).is_err()
        {
            if let Ok(eth) = EthernetFrame::new_checked(maybe_ip) {
                match eth.ethertype() {
                    EthernetProtocol::Ipv4 | EthernetProtocol::Ipv6 => {
                        maybe_ip = eth.payload();
                    }
                    _ => {}
                }
            }
        }

        if let Ok(pkt4) = Ipv4Packet::new_checked(maybe_ip) {
            if pkt4.next_header() != IpProtocol::Tcp {
                return false;
            }
            if let Ok(tcp) = TcpPacket::new_checked(pkt4.payload()) {
                dst_port = Some(tcp.dst_port());
                is_pure_syn = tcp.syn() && !tcp.ack();
            }
        } else if let Ok(pkt6) = Ipv6Packet::new_checked(maybe_ip) {
            // IPv6 可能包含扩展头，这里做一个保守跳过：能到 TCP 则解析，否则不丢。
            let data = pkt6.as_ref();
            if data.len() < 40 {
                return false;
            }
            let mut next = data[6];
            let mut off = 40usize;
            loop {
                if next == IpProtocol::Tcp.into() {
                    if off >= data.len() {
                        return false;
                    }
                    if let Ok(tcp) = TcpPacket::new_checked(&data[off..]) {
                        dst_port = Some(tcp.dst_port());
                        is_pure_syn = tcp.syn() && !tcp.ack();
                    }
                    break;
                }
                match next {
                    // Hop-by-Hop / Routing / Destination Options: [next][hdr_ext_len]...
                    0 | 43 | 60 => {
                        if off + 2 > data.len() {
                            return false;
                        }
                        let nh = data[off];
                        let hdr_ext_len = data[off + 1] as usize;
                        let hdr_len = (hdr_ext_len + 1) * 8;
                        if off + hdr_len > data.len() {
                            return false;
                        }
                        next = nh;
                        off += hdr_len;
                    }
                    // Fragment header: fixed 8 bytes, [next] at first byte.
                    44 => {
                        if off + 8 > data.len() {
                            return false;
                        }
                        let nh = data[off];
                        next = nh;
                        off += 8;
                    }
                    _ => {
                        // 未知扩展头：不丢
                        return false;
                    }
                }
            }
        } else {
            return false;
        }

        let port = match dst_port {
            Some(p) => p,
            None => return false,
        };
        if !is_pure_syn {
            return false;
        }

        // backlog==0 策略：同一轮 poll 内只允许第一个 SYN 通过，其余纯 SYN 丢弃以避免 RST。
        let mut guard = self.ports.write();
        let Some(entry) = guard.iter_mut().find(|e| e.port == port) else {
            return false;
        };
        if !entry.drop_syn_when_full {
            return false;
        }

        if entry.listen_socket_present {
            // 允许第一个 SYN 通过，然后在本轮剩余时间里视作“无 LISTEN socket”，让后续 SYN 被丢弃。
            entry.listen_socket_present = false;
            false
        } else {
            true
        }
    }
}
