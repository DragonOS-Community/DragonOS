use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use jhash::jhash2;
use smoltcp::wire::{IpAddress, IpEndpoint};

use crate::libs::rwsem::RwSem;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::namespace::NamespaceOps;

use super::UdpSocket;

#[derive(Clone)]
struct UdpBinding {
    netns_id: usize,
    socket: Weak<UdpSocket>,
    addr: IpAddress,
    port: u16,
    reuseport: bool,
    bound_seq: u64,
}

#[derive(Debug, Clone)]
struct UdpBindingMatch {
    socket: Arc<UdpSocket>,
    reuseport: bool,
    bound_seq: u64,
}

static BIND_SEQ: AtomicU64 = AtomicU64::new(1);

lazy_static! {
    static ref UDP_BINDINGS: RwSem<Vec<UdpBinding>> = RwSem::new(Vec::new());
}

pub fn register_udp_binding(
    netns: &Arc<NetNamespace>,
    socket: Weak<UdpSocket>,
    addr: IpAddress,
    port: u16,
    _reuseaddr: bool,
    reuseport: bool,
) {
    let netns_id = netns.ns_common().nsid.data();
    let bound_seq = BIND_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut guard = UDP_BINDINGS.write();
    guard.push(UdpBinding {
        netns_id,
        socket,
        addr,
        port,
        reuseport,
        bound_seq,
    });
    guard.retain(|b| b.socket.strong_count() > 0);
}

pub fn unregister_udp_binding(netns: &Arc<NetNamespace>, socket: &Weak<UdpSocket>) {
    let netns_id = netns.ns_common().nsid.data();
    let mut guard = UDP_BINDINGS.write();
    guard.retain(|b| b.netns_id != netns_id || b.socket.as_ptr() != socket.as_ptr());
    guard.retain(|b| b.socket.strong_count() > 0);
}

pub fn deliver_unicast_loopback(
    netns: &Arc<NetNamespace>,
    dest: IpEndpoint,
    src: IpEndpoint,
    ifindex: i32,
    payload: &[u8],
) -> usize {
    let candidates = match_udp_bindings(netns, dest.addr, dest.port);
    if candidates.is_empty() {
        return 0;
    }

    let chosen = if candidates.iter().any(|c| c.reuseport) {
        choose_reuseport_socket(&candidates, dest, src)
    } else {
        choose_recent_socket(&candidates)
    };

    if let Some(sock) = chosen {
        if sock.inject_loopback_packet(src, dest.addr, dest.port, ifindex, payload) {
            return 1;
        }
    }
    0
}

pub fn deliver_multicast_all(
    netns: &Arc<NetNamespace>,
    dest: IpEndpoint,
    src: IpEndpoint,
    ifindex: i32,
    payload: &[u8],
) -> usize {
    let candidates = match_udp_bindings(netns, dest.addr, dest.port);
    if candidates.is_empty() {
        return 0;
    }
    let multiaddr = match dest.addr {
        IpAddress::Ipv4(addr) => {
            let octets = addr.octets();
            u32::from_ne_bytes(octets)
        }
        _ => return 0,
    };
    let mut delivered = 0;
    for cand in candidates {
        let multicast_all = cand.socket.ip_multicast_all.load(Ordering::Relaxed);
        if !multicast_all
            && !cand
                .socket
                .has_ipv4_multicast_membership(multiaddr, ifindex)
        {
            continue;
        }
        if cand
            .socket
            .inject_loopback_packet(src, dest.addr, dest.port, ifindex, payload)
        {
            delivered += 1;
        }
    }
    delivered
}

pub fn deliver_broadcast_all(
    netns: &Arc<NetNamespace>,
    dest: IpEndpoint,
    src: IpEndpoint,
    ifindex: i32,
    payload: &[u8],
) -> usize {
    let candidates = match_udp_bindings(netns, dest.addr, dest.port);
    if candidates.is_empty() {
        return 0;
    }
    let mut delivered = 0;
    for cand in candidates {
        if cand
            .socket
            .inject_loopback_packet(src, dest.addr, dest.port, ifindex, payload)
        {
            delivered += 1;
        }
    }
    delivered
}

fn match_udp_bindings(
    netns: &Arc<NetNamespace>,
    dest_addr: IpAddress,
    dest_port: u16,
) -> Vec<UdpBindingMatch> {
    let netns_id = netns.ns_common().nsid.data();
    let mut guard = UDP_BINDINGS.write();
    guard.retain(|b| b.socket.strong_count() > 0);
    guard
        .iter()
        .filter(|b| b.netns_id == netns_id)
        .filter(|b| b.port == dest_port)
        .filter(|b| udp_addr_match(b.addr, dest_addr))
        .filter_map(|b| {
            b.socket.upgrade().map(|sock| UdpBindingMatch {
                socket: sock,
                reuseport: b.reuseport,
                bound_seq: b.bound_seq,
            })
        })
        .collect()
}

#[inline]
fn udp_addr_match(bound_addr: IpAddress, dest_addr: IpAddress) -> bool {
    if bound_addr.version() != dest_addr.version() {
        return false;
    }
    if bound_addr.is_unspecified() {
        return true;
    }
    if dest_addr.is_multicast() || dest_addr.is_broadcast() {
        return true;
    }
    bound_addr == dest_addr
}

fn choose_recent_socket(candidates: &[UdpBindingMatch]) -> Option<Arc<UdpSocket>> {
    candidates
        .iter()
        .max_by_key(|c| c.bound_seq)
        .map(|c| c.socket.clone())
}

fn choose_reuseport_socket(
    candidates: &[UdpBindingMatch],
    dest: IpEndpoint,
    src: IpEndpoint,
) -> Option<Arc<UdpSocket>> {
    let reuseport: Vec<&UdpBindingMatch> = candidates.iter().filter(|c| c.reuseport).collect();
    if reuseport.is_empty() {
        return None;
    }

    let hash = udp_4tuple_hash(dest, src);
    let idx = (hash as usize) % reuseport.len();
    reuseport.get(idx).map(|c| c.socket.clone())
}

fn udp_4tuple_hash(dest: IpEndpoint, src: IpEndpoint) -> u32 {
    let src_port = src.port as u32;
    let dst_port = dest.port as u32;
    match (dest.addr, src.addr) {
        (IpAddress::Ipv4(dst), IpAddress::Ipv4(src)) => {
            let data = [src.to_bits(), dst.to_bits(), src_port, dst_port];
            jhash2(&data, 0)
        }
        (IpAddress::Ipv6(dst), IpAddress::Ipv6(src)) => {
            let src_oct = src.octets();
            let dst_oct = dst.octets();
            let data = [
                u32::from_be_bytes([src_oct[0], src_oct[1], src_oct[2], src_oct[3]]),
                u32::from_be_bytes([src_oct[4], src_oct[5], src_oct[6], src_oct[7]]),
                u32::from_be_bytes([dst_oct[0], dst_oct[1], dst_oct[2], dst_oct[3]]),
                u32::from_be_bytes([dst_oct[4], dst_oct[5], dst_oct[6], dst_oct[7]]),
                src_port,
                dst_port,
            ];
            jhash2(&data, 0)
        }
        _ => {
            let data = [src_port, dst_port, 0, 0];
            jhash2(&data, 0)
        }
    }
}
