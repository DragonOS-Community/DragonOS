use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, AtomicU64, Ordering};

use hashbrown::HashMap;
use jhash::jhash2;
use smoltcp::{
    phy::PacketMeta,
    wire::{IpAddress, IpEndpoint},
};
use system_error::SystemError;

use crate::arch::rand::rand;
use crate::libs::mutex::Mutex;

use super::UdpSocket;
use crate::process::namespace::net_namespace::NetNamespace;

#[derive(Debug)]
pub(crate) struct NetnsUdpIngress {
    netns: Weak<NetNamespace>,
    ifindex: i32,
}

impl NetnsUdpIngress {
    pub fn try_new(netns: &Arc<NetNamespace>, ifindex: usize) -> Result<Arc<Self>, SystemError> {
        Arc::try_new(Self {
            netns: Arc::downgrade(netns),
            ifindex: ifindex as i32,
        })
        .map_err(|_| SystemError::ENOMEM)
    }
}

impl smoltcp::iface::UdpIngressHandler for NetnsUdpIngress {
    fn handle_udp_ingress(
        &self,
        meta: PacketMeta,
        ip_repr: &smoltcp::wire::IpRepr,
        udp_repr: &smoltcp::wire::UdpRepr,
        is_broadcast: bool,
        payload: &[u8],
    ) -> smoltcp::iface::UdpIngressResult {
        let smoltcp::wire::IpRepr::Ipv4(ipv4) = ip_repr else {
            return smoltcp::iface::UdpIngressResult::NotHandled;
        };
        let Some(netns) = self.netns.upgrade() else {
            return smoltcp::iface::UdpIngressResult::NotHandled;
        };
        let src = IpEndpoint::new(IpAddress::Ipv4(ipv4.src_addr), udp_repr.src_port);
        let dest = IpEndpoint::new(IpAddress::Ipv4(ipv4.dst_addr), udp_repr.dst_port);
        // Namespace-local handoff preserves Linux's skb_iif in PacketMeta.
        // Physical ingress uses the handler owner's ifindex as the fallback.
        // Socket device binding must match the original ingress device, not
        // the smoltcp instance that performs transport demultiplexing.
        let ingress_ifindex = i32::try_from(meta.id)
            .ok()
            .filter(|ifindex| *ifindex > 0)
            .unwrap_or(self.ifindex);
        if netns.udp_bindings().deliver_ingress(
            dest,
            src,
            ingress_ifindex,
            is_broadcast,
            meta,
            payload,
        ) {
            smoltcp::iface::UdpIngressResult::Consumed
        } else {
            smoltcp::iface::UdpIngressResult::NotHandled
        }
    }
}

#[derive(Debug, Clone)]
struct UdpBinding {
    socket: Weak<UdpSocket>,
    addr: IpAddress,
    /// Bind-time reuseport group membership. Unlike SO_REUSEADDR conflict
    /// checks, Linux reuseport delivery membership is not the live option bit.
    reuseport: bool,
    bind_id: usize,
    generation: u64,
    bound_seq: u64,
}

#[derive(Debug, Clone)]
struct UdpBindingMatch {
    socket: Arc<UdpSocket>,
    reuseport: bool,
    generation: u64,
    bound_seq: u64,
    score: UdpLookupScore,
}

/// The subset of Linux `compute_score()` that DragonOS currently models.
///
/// An exact local address is ordered first because Linux searches the
/// destination-address hash before its wildcard hash. Within that class, a
/// connected four-tuple outranks an unconnected socket and SO_BINDTODEVICE
/// outranks an otherwise equivalent device-wildcard socket.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct UdpLookupScore {
    local_exact: bool,
    connected: bool,
    device_bound: bool,
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
        generation: u64,
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
            generation,
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
        generation: u64,
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
                generation,
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
        let Ok(candidates) = self.match_bindings(dest.addr, dest.port, src, ifindex) else {
            return 0;
        };
        let chosen = choose_unicast_socket(&candidates, dest, src);
        chosen
            .filter(|candidate| {
                candidate.socket.inject_loopback_packet(
                    candidate.generation,
                    src,
                    dest.addr,
                    dest.port,
                    ifindex,
                    payload,
                )
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
        let Ok(candidates) = self.match_bindings(dest.addr, dest.port, src, ifindex) else {
            return 0;
        };
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
                candidate.socket.inject_loopback_packet(
                    candidate.generation,
                    src,
                    dest.addr,
                    dest.port,
                    ifindex,
                    payload,
                )
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
        let Ok(candidates) = self.match_bindings(dest.addr, dest.port, src, ifindex) else {
            return 0;
        };
        candidates
            .into_iter()
            .filter(|candidate| {
                candidate.socket.inject_loopback_packet(
                    candidate.generation,
                    src,
                    dest.addr,
                    dest.port,
                    ifindex,
                    payload,
                )
            })
            .count()
    }

    /// Delivers one UDP datagram after smoltcp has validated the IP/UDP packet.
    /// `true` means a Linux UDP binding consumed the protocol lookup, including
    /// the normal receive-buffer-full drop case, so smoltcp must not emit ICMP.
    pub fn deliver_ingress(
        &self,
        dest: IpEndpoint,
        src: IpEndpoint,
        ifindex: i32,
        is_broadcast: bool,
        meta: PacketMeta,
        payload: &[u8],
    ) -> bool {
        let candidates = match self.match_bindings(dest.addr, dest.port, src, ifindex) {
            Ok(candidates) => candidates,
            // A matching bucket exists but its fallible snapshot could not be
            // allocated. UDP drops under receive-memory pressure without ICMP.
            Err(()) => return true,
        };
        if candidates.is_empty() {
            return false;
        }
        if dest.addr.is_multicast() {
            let IpAddress::Ipv4(addr) = dest.addr else {
                return false;
            };
            let multiaddr = u32::from_ne_bytes(addr.octets());
            for candidate in candidates {
                if candidate.socket.ip_multicast_all.load(Ordering::Relaxed)
                    || candidate
                        .socket
                        .has_ipv4_multicast_membership(multiaddr, ifindex)
                {
                    candidate.socket.inject_ingress_packet(
                        candidate.generation,
                        src,
                        dest.addr,
                        ifindex,
                        meta,
                        payload,
                    );
                }
            }
        } else if is_broadcast {
            for candidate in candidates {
                candidate.socket.inject_ingress_packet(
                    candidate.generation,
                    src,
                    dest.addr,
                    ifindex,
                    meta,
                    payload,
                );
            }
        } else {
            let chosen = choose_unicast_socket(&candidates, dest, src);
            if let Some(socket) = chosen {
                socket.socket.inject_ingress_packet(
                    socket.generation,
                    src,
                    dest.addr,
                    ifindex,
                    meta,
                    payload,
                );
            }
        }
        true
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
        source: IpEndpoint,
        ingress_ifindex: i32,
    ) -> Result<Vec<UdpBindingMatch>, ()> {
        let mut bindings = self.bindings.lock();
        let Some(bucket) = bindings.get_mut(&dest_port) else {
            return Ok(Vec::new());
        };
        Self::cleanup_bucket(bucket);
        let mut matches = Vec::new();
        for binding in bucket.iter() {
            if !udp_addr_match(binding.addr, dest_addr) {
                continue;
            }
            let Some(socket) = binding.socket.upgrade() else {
                continue;
            };
            let bound_ifindex = socket.bound_device_ifindex();
            if ingress_ifindex <= 0
                || (bound_ifindex != 0 && bound_ifindex != ingress_ifindex as usize)
            {
                continue;
            }
            let Some(connected) = socket.ingress_match(binding.generation, source, dest_addr)
            else {
                continue;
            };
            matches.try_reserve(1).map_err(|_| ())?;
            matches.push(UdpBindingMatch {
                socket,
                reuseport: binding.reuseport,
                generation: binding.generation,
                bound_seq: binding.bound_seq,
                score: UdpLookupScore {
                    local_exact: !binding.addr.is_unspecified(),
                    connected,
                    device_bound: bound_ifindex != 0,
                },
            });
        }
        Ok(matches)
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
    bound_addr.is_unspecified() || bound_addr == dest_addr
}

fn choose_unicast_socket(
    candidates: &[UdpBindingMatch],
    dest: IpEndpoint,
    src: IpEndpoint,
) -> Option<&UdpBindingMatch> {
    let best_score = candidates.iter().map(|candidate| candidate.score).max()?;
    let primary = candidates
        .iter()
        .filter(|candidate| candidate.score == best_score)
        .max_by_key(|candidate| candidate.bound_seq)
        .unwrap();
    if !primary.reuseport {
        return Some(primary);
    }

    // Linux selects a best-scoring socket first and only then applies
    // SO_REUSEPORT within that equivalent lookup class. In particular, a
    // lower-specificity reuseport socket must never divert the datagram.
    let count = candidates
        .iter()
        .filter(|candidate| candidate.score == best_score && candidate.reuseport)
        .count();
    let index = (udp_4tuple_hash(dest, src) as usize) % count;
    candidates
        .iter()
        .filter(|candidate| candidate.score == best_score && candidate.reuseport)
        .nth(index)
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
