use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, AtomicU64, Ordering};

use hashbrown::HashMap;
use jhash::jhash2;
use smoltcp::wire::{IpAddress, IpEndpoint};
use system_error::SystemError;

use crate::arch::rand::rand;
use crate::libs::mutex::Mutex;

use super::UdpSocket;

#[derive(Debug, Clone)]
struct UdpBinding {
    socket: Weak<UdpSocket>,
    addr: IpAddress,
    /// Bind-time reuseport group membership. Unlike SO_REUSEADDR conflict
    /// checks, Linux reuseport delivery membership is not the live option bit.
    reuseport: bool,
    bind_id: usize,
    bound_seq: u64,
}

#[derive(Debug, Clone)]
struct UdpBindingMatch {
    socket: Arc<UdpSocket>,
    reuseport: bool,
    bound_seq: u64,
}

/// Per-network-namespace UDP port reservation and local-delivery table.
///
/// Device binding is intentionally not cached in an entry. Conflict checks and
/// delivery read each socket's authoritative `SocketDeviceBinding` so changing
/// SO_BINDTODEVICE cannot leave a stale port-table projection.
#[derive(Debug)]
pub struct UdpBindingTable {
    bindings: Mutex<HashMap<u16, Vec<UdpBinding>>>,
    next_ephemeral: AtomicU16,
    bind_seq: AtomicU64,
}

impl Default for UdpBindingTable {
    fn default() -> Self {
        Self {
            bindings: Mutex::new(HashMap::new()),
            next_ephemeral: AtomicU16::new(0),
            bind_seq: AtomicU64::new(1),
        }
    }
}

impl UdpBindingTable {
    #[allow(clippy::too_many_arguments)]
    pub fn bind(
        &self,
        socket: Weak<UdpSocket>,
        addr: IpAddress,
        port: u16,
        reuseaddr: bool,
        reuseport: bool,
        bind_id: usize,
        prospective_ifindex: usize,
    ) -> Result<(), SystemError> {
        if port == 0 {
            return Err(SystemError::EINVAL);
        }
        let mut bindings = self.bindings.lock();
        let bucket = bindings.entry(port).or_default();
        Self::cleanup_bucket(bucket);
        if Self::conflicts(
            bucket,
            addr,
            reuseaddr,
            reuseport,
            prospective_ifindex,
            bind_id,
        ) {
            return Err(SystemError::EADDRINUSE);
        }
        bucket.push(UdpBinding {
            socket,
            addr,
            reuseport,
            bind_id,
            bound_seq: self.bind_seq.fetch_add(1, Ordering::Relaxed),
        });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn bind_ephemeral(
        &self,
        socket: Weak<UdpSocket>,
        addr: IpAddress,
        reuseaddr: bool,
        reuseport: bool,
        bind_id: usize,
        prospective_ifindex: usize,
        range: (u16, u16),
    ) -> Result<u16, SystemError> {
        let (min, max) = range;
        if min == 0 || max == 0 || min > max {
            return Err(SystemError::EINVAL);
        }
        let count = (max - min) as u32 + 1;
        let current = self.next_ephemeral.load(Ordering::Relaxed);
        if current < min || current > max {
            self.next_ephemeral
                .store(min + (rand() % count as usize) as u16, Ordering::Relaxed);
        }

        let mut bindings = self.bindings.lock();
        for _ in 0..count {
            let old = self
                .next_ephemeral
                .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |cur| {
                    let cur = if cur < min || cur > max { min } else { cur };
                    Some(if cur >= max { min } else { cur + 1 })
                })
                .unwrap_or_else(|cur| cur);
            let port = if old < min || old >= max {
                min
            } else {
                old + 1
            };
            let bucket = bindings.entry(port).or_default();
            Self::cleanup_bucket(bucket);
            if Self::conflicts(
                bucket,
                addr,
                reuseaddr,
                reuseport,
                prospective_ifindex,
                bind_id,
            ) {
                continue;
            }
            bucket.push(UdpBinding {
                socket,
                addr,
                reuseport,
                bind_id,
                bound_seq: self.bind_seq.fetch_add(1, Ordering::Relaxed),
            });
            return Ok(port);
        }
        Err(SystemError::EADDRINUSE)
    }

    pub fn unbind(&self, port: u16, bind_id: usize) {
        let mut bindings = self.bindings.lock();
        let remove_bucket = if let Some(bucket) = bindings.get_mut(&port) {
            bucket
                .retain(|binding| binding.bind_id != bind_id && binding.socket.strong_count() > 0);
            bucket.is_empty()
        } else {
            false
        };
        if remove_bucket {
            bindings.remove(&port);
        }
    }

    pub fn deliver_unicast(
        &self,
        dest: IpEndpoint,
        src: IpEndpoint,
        ifindex: i32,
        payload: &[u8],
    ) -> usize {
        let candidates = self.match_bindings(dest.addr, dest.port, ifindex);
        let chosen = if candidates.iter().any(|candidate| candidate.reuseport) {
            choose_reuseport_socket(&candidates, dest, src)
        } else {
            choose_recent_socket(&candidates)
        };
        chosen
            .filter(|socket| {
                socket.inject_loopback_packet(src, dest.addr, dest.port, ifindex, payload)
            })
            .map_or(0, |_| 1)
    }

    pub fn deliver_multicast(
        &self,
        dest: IpEndpoint,
        src: IpEndpoint,
        ifindex: i32,
        payload: &[u8],
    ) -> usize {
        let candidates = self.match_bindings(dest.addr, dest.port, ifindex);
        let multiaddr = match dest.addr {
            IpAddress::Ipv4(addr) => u32::from_ne_bytes(addr.octets()),
            _ => return 0,
        };
        candidates
            .into_iter()
            .filter(|candidate| {
                candidate.socket.ip_multicast_all.load(Ordering::Relaxed)
                    || candidate
                        .socket
                        .has_ipv4_multicast_membership(multiaddr, ifindex)
            })
            .filter(|candidate| {
                candidate
                    .socket
                    .inject_loopback_packet(src, dest.addr, dest.port, ifindex, payload)
            })
            .count()
    }

    pub fn deliver_broadcast(
        &self,
        dest: IpEndpoint,
        src: IpEndpoint,
        ifindex: i32,
        payload: &[u8],
    ) -> usize {
        self.match_bindings(dest.addr, dest.port, ifindex)
            .into_iter()
            .filter(|candidate| {
                candidate
                    .socket
                    .inject_loopback_packet(src, dest.addr, dest.port, ifindex, payload)
            })
            .count()
    }

    #[allow(clippy::too_many_arguments)]
    fn conflicts(
        bindings: &[UdpBinding],
        addr: IpAddress,
        reuseaddr: bool,
        reuseport: bool,
        prospective_ifindex: usize,
        bind_id: usize,
    ) -> bool {
        bindings.iter().any(|binding| {
            if binding.bind_id == bind_id || !udp_addrs_conflict(binding.addr, addr) {
                return false;
            }
            let Some(socket) = binding.socket.upgrade() else {
                return false;
            };
            let existing_ifindex = socket.bound_device_ifindex();
            if existing_ifindex != 0
                && prospective_ifindex != 0
                && existing_ifindex != prospective_ifindex
            {
                return false;
            }
            let (existing_reuseaddr, _) = socket.reuse_options();
            !((reuseport && binding.reuseport) || (reuseaddr && existing_reuseaddr))
        })
    }

    fn match_bindings(
        &self,
        dest_addr: IpAddress,
        dest_port: u16,
        ingress_ifindex: i32,
    ) -> Vec<UdpBindingMatch> {
        let mut bindings = self.bindings.lock();
        let Some(bucket) = bindings.get_mut(&dest_port) else {
            return Vec::new();
        };
        Self::cleanup_bucket(bucket);
        bucket
            .iter()
            .filter(|binding| udp_addr_match(binding.addr, dest_addr))
            .filter_map(|binding| {
                let socket = binding.socket.upgrade()?;
                if ingress_ifindex <= 0 || !socket.device_binding_allows(ingress_ifindex as usize) {
                    return None;
                }
                Some(UdpBindingMatch {
                    socket,
                    reuseport: binding.reuseport,
                    bound_seq: binding.bound_seq,
                })
            })
            .collect()
    }

    fn cleanup_bucket(bindings: &mut Vec<UdpBinding>) {
        bindings.retain(|binding| binding.socket.strong_count() > 0);
    }
}

#[inline]
fn udp_addrs_conflict(a: IpAddress, b: IpAddress) -> bool {
    a.version() == b.version() && (a.is_unspecified() || b.is_unspecified() || a == b)
}

#[inline]
fn udp_addr_match(bound_addr: IpAddress, dest_addr: IpAddress) -> bool {
    if bound_addr.version() != dest_addr.version() {
        return false;
    }
    bound_addr.is_unspecified()
        || dest_addr.is_multicast()
        || dest_addr.is_broadcast()
        || bound_addr == dest_addr
}

fn choose_recent_socket(candidates: &[UdpBindingMatch]) -> Option<Arc<UdpSocket>> {
    candidates
        .iter()
        .max_by_key(|candidate| candidate.bound_seq)
        .map(|candidate| candidate.socket.clone())
}

fn choose_reuseport_socket(
    candidates: &[UdpBindingMatch],
    dest: IpEndpoint,
    src: IpEndpoint,
) -> Option<Arc<UdpSocket>> {
    let reuseport: Vec<&UdpBindingMatch> = candidates
        .iter()
        .filter(|candidate| candidate.reuseport)
        .collect();
    if reuseport.is_empty() {
        return None;
    }
    let index = (udp_4tuple_hash(dest, src) as usize) % reuseport.len();
    reuseport
        .get(index)
        .map(|candidate| candidate.socket.clone())
}

fn udp_4tuple_hash(dest: IpEndpoint, src: IpEndpoint) -> u32 {
    let src_port = src.port as u32;
    let dst_port = dest.port as u32;
    match (dest.addr, src.addr) {
        (IpAddress::Ipv4(dst), IpAddress::Ipv4(src)) => {
            jhash2(&[src.to_bits(), dst.to_bits(), src_port, dst_port], 0)
        }
        (IpAddress::Ipv6(dst), IpAddress::Ipv6(src)) => {
            let src_octets = src.octets();
            let dst_octets = dst.octets();
            jhash2(
                &[
                    u32::from_be_bytes(src_octets[0..4].try_into().unwrap()),
                    u32::from_be_bytes(src_octets[4..8].try_into().unwrap()),
                    u32::from_be_bytes(dst_octets[0..4].try_into().unwrap()),
                    u32::from_be_bytes(dst_octets[4..8].try_into().unwrap()),
                    src_port,
                    dst_port,
                ],
                0,
            )
        }
        _ => jhash2(&[src_port, dst_port, 0, 0], 0),
    }
}
