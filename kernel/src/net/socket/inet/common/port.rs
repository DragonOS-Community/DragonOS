use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use hashbrown::HashMap;
use smoltcp::wire::{IpAddress, IpVersion};
use system_error::SystemError;

use crate::{arch::rand::rand, libs::mutex::Mutex, process::ProcessManager};

use super::Types::{self, *};

/// Network-namespace-wide TCP port manager.
/// UDP reservations are managed separately by `UdpBindingTable`.
#[derive(Debug)]
pub struct PortManager {
    // TCP port table. One port may have multiple bindings shared by SO_REUSEPORT/SO_REUSEADDR.
    tcp_port_table: Mutex<HashMap<u16, TcpPortBucket>>,
}

impl Default for PortManager {
    fn default() -> Self {
        Self {
            tcp_port_table: Mutex::new(HashMap::new()),
        }
    }
}

pub const DEFAULT_LOCAL_PORT_RANGE: u32 = (32768u32 << 16) | 60999u32;

impl PortManager {
    pub fn local_port_range() -> (u16, u16) {
        ProcessManager::current_netns().local_port_range()
    }

    pub fn set_local_port_range(min: u16, max: u16) -> Result<(), SystemError> {
        ProcessManager::current_netns().set_local_port_range(min, max)
    }

    /// @brief Automatically allocate an unused port for the requested protocol. Returns EADDRINUSE if all ephemeral ports are occupied.
    pub fn get_ephemeral_port(&self, socket_type: Types) -> Result<u16, SystemError> {
        // TODO: selects non-conflict high port
        static EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(0);
        let (min, max) = Self::local_port_range();
        let range = (max - min) as u32 + 1;
        if range == 0 {
            return Err(SystemError::EINVAL);
        }
        let current = EPHEMERAL_PORT.load(Ordering::Relaxed);
        if current < min || current > max {
            let initial = min + (rand() % range as usize) as u16;
            EPHEMERAL_PORT.store(initial, Ordering::Relaxed);
        }

        let mut remaining = range;
        while remaining > 0 {
            let old = EPHEMERAL_PORT
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

            // Check whether the port is already occupied through the port table
            match socket_type {
                Tcp => {
                    let guard = self.tcp_port_table.lock();
                    if guard.get(&port).is_none() {
                        drop(guard);
                        return Ok(port);
                    }
                }
                _ => panic!("{:?} cann't get a port", socket_type),
            }
            remaining -= 1;
        }
        return Err(SystemError::EADDRINUSE);
    }

    #[inline]
    pub fn bind_tcp_ephemeral_port(
        &self,
        addr: IpAddress,
        family: IpVersion,
        reuseaddr: bool,
        reuseport: bool,
        uid: u32,
        id: TcpBindId,
    ) -> Result<u16, SystemError> {
        let (min, max) = Self::local_port_range();
        let range = (max - min) as u32 + 1;
        if range == 0 {
            return Err(SystemError::EINVAL);
        }
        let mut remaining = range;
        while remaining > 0 {
            let port = self.get_ephemeral_port(Types::Tcp)?;
            match self.bind_tcp_port(port, addr, family, reuseaddr, reuseport, uid, id) {
                Ok(()) => return Ok(port),
                Err(SystemError::EADDRINUSE) => {
                    // Race: another thread grabbed the port after we checked.
                    remaining -= 1;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Err(SystemError::EADDRINUSE)
    }

    /// Bind a TCP endpoint in the network namespace using a stable socket identity.
    pub fn bind_tcp_port(
        &self,
        port: u16,
        addr: IpAddress,
        family: IpVersion,
        reuseaddr: bool,
        reuseport: bool,
        uid: u32,
        id: TcpBindId,
    ) -> Result<(), SystemError> {
        if port == 0 {
            return Err(SystemError::EINVAL);
        }
        let member = TcpPortBinding {
            addr,
            family,
            reuseaddr,
            reuseport,
            uid,
            id,
            listening: false,
            group: None,
            listen_order: 0,
            hash_tail: false,
        };
        let mut guard = self.tcp_port_table.lock();
        if let Some(bucket) = guard.get(&port) {
            if bucket.members.iter().any(|b| b.id == id) {
                return Err(SystemError::EINVAL);
            }
            bucket.check_conflict(&member)?;
        }
        // Do not leave empty buckets behind on failed admission.
        let bucket = guard.entry(port).or_default();
        bucket.update_fastreuse(&member);
        bucket.members.push(member);
        Ok(())
    }

    /// Reserve admission before publishing LISTEN slots. On construction failure,
    /// the caller must stop the reservation before returning to Bound.
    pub fn listen_tcp_port(&self, port: u16, id: TcpBindId) -> Result<(), SystemError> {
        let mut guard = self.tcp_port_table.lock();
        let bucket = guard.get_mut(&port).ok_or(SystemError::EINVAL)?;
        let index = bucket
            .members
            .iter()
            .position(|b| b.id == id)
            .ok_or(SystemError::EINVAL)?;
        let mut candidate = bucket.members[index].clone();
        candidate.listening = true;
        bucket.check_conflict(&candidate)?;
        bucket.update_fastreuse(&candidate);
        if !bucket.members[index].listening {
            static LISTEN_ORDER: AtomicU64 = AtomicU64::new(1);
            candidate.listen_order = LISTEN_ORDER.fetch_add(1, Ordering::Relaxed);
            candidate.hash_tail = candidate.family == IpVersion::Ipv6 && candidate.reuseport;
            if candidate.reuseport {
                // Linux chooses an existing group through a currently reuseport-
                // enabled listener; membership itself survives option changes.
                if let Some(peer) = bucket
                    .members
                    .iter_mut()
                    .filter(|m| {
                        m.id != id
                            && m.listening
                            && m.reuseport
                            && m.uid == candidate.uid
                            && m.addr == candidate.addr
                            && m.family == candidate.family
                    })
                    .max_by_key(|m| m.lookup_order())
                {
                    let group = *peer.group.get_or_insert(peer.id);
                    candidate.group = Some(group);
                } else {
                    candidate.group = Some(id);
                }
            }
        }
        bucket.members[index] = candidate;
        Ok(())
    }

    pub fn stop_tcp_listen(&self, port: u16, id: TcpBindId) {
        let mut guard = self.tcp_port_table.lock();
        if let Some(bucket) = guard.get_mut(&port) {
            if let Some(member) = bucket.members.iter_mut().find(|b| b.id == id) {
                member.listening = false;
                member.group = None;
                member.listen_order = 0;
            }
        }
    }

    /// Linux updates sk_reuse/sk_reuseport without rewriting the bind bucket's
    /// fastreuse cache. Admission refreshes that cache at bind/listen time.
    pub fn update_tcp_options(
        &self,
        port: u16,
        id: TcpBindId,
        reuseaddr: Option<bool>,
        reuseport: Option<bool>,
    ) -> Result<(), SystemError> {
        let mut guard = self.tcp_port_table.lock();
        let member = guard
            .get_mut(&port)
            .and_then(|b| b.members.iter_mut().find(|m| m.id == id))
            .ok_or(SystemError::EINVAL)?;
        if let Some(value) = reuseaddr {
            member.reuseaddr = value;
        }
        if let Some(value) = reuseport {
            member.reuseport = value;
        }
        Ok(())
    }

    /// Called with the interface's SocketSet locked, immediately before ingress.
    /// No port-manager operation acquires SocketSet, so this lock order is one-way.
    /// The protocol stack sees only opaque IDs and admission, never Linux groups.
    pub fn refresh_tcp_listener_selection(&self, sockets: &mut smoltcp::iface::SocketSet<'_>) {
        let guard = self.tcp_port_table.lock();
        for item in sockets.items_mut() {
            if let smoltcp::socket::Socket::Tcp(socket) = &mut item.socket {
                if let Some(id) = socket.listener_id() {
                    let enabled = guard
                        .get(&socket.listen_endpoint().port)
                        .map_or(false, |bucket| bucket.listener_is_selected(id));
                    socket.set_listener_enabled(enabled);
                }
            }
        }
    }

    /// Remove exactly this binding, independent of accept's handle replacements.
    pub fn unbind_tcp_port(&self, port: u16, id: TcpBindId) {
        let mut guard = self.tcp_port_table.lock();
        if let Some(bucket) = guard.get_mut(&port) {
            bucket.members.retain(|b| b.id != id);
            if bucket.members.is_empty() {
                guard.remove(&port);
            }
        }
    }
}

/// A socket binding's identity does not change when a backlog slot is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpBindId(u64);

impl TcpBindId {
    pub fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self(
            NEXT_ID
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .expect("TCP binding identity space exhausted"),
        )
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone)]
struct TcpPortBinding {
    addr: IpAddress,
    // Socket family survives IPv4-mapped address normalization.
    family: IpVersion,
    reuseaddr: bool,
    reuseport: bool,
    uid: u32,
    id: TcpBindId,
    listening: bool,
    group: Option<TcpBindId>,
    listen_order: u64,
    // Linux inserts IPv6 reuseport listeners at the tail; other listeners at the head.
    hash_tail: bool,
}

impl TcpPortBinding {
    fn lookup_order(&self) -> (bool, u64) {
        (
            !self.hash_tail,
            if self.hash_tail {
                u64::MAX - self.listen_order
            } else {
                self.listen_order
            },
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct FastReusePort {
    uid: u32,
    addr: IpAddress,
    strict: bool,
}

#[derive(Debug, Default)]
struct TcpPortBucket {
    members: Vec<TcpPortBinding>,
    fast_reuseaddr: bool,
    fast_reuseport: Option<FastReusePort>,
}

impl TcpPortBucket {
    fn listener_is_selected(&self, id: u64) -> bool {
        let Some(member) = self
            .members
            .iter()
            .find(|m| m.id.get() == id && m.listening)
        else {
            return false;
        };
        // Exact and wildcard address layers are resolved by the stack. Within
        // one layer Linux prefers AF_INET to mapped AF_INET6, then uses
        // hash-list order. Groups never cross socket families.
        let first = self
            .members
            .iter()
            .filter(|m| m.listening && m.addr == member.addr)
            .max_by_key(|m| (m.family == IpVersion::Ipv4, m.lookup_order()))
            .expect("active member must have a listener in its address layer");
        if first.reuseport {
            match first.group {
                Some(group) => member.group == Some(group),
                None => member.id == first.id,
            }
        } else {
            member.id == first.id
        }
    }

    fn fast_reuseport_matches(&self, candidate: &TcpPortBinding) -> bool {
        self.fast_reuseport.map_or(false, |cached| {
            candidate.reuseport
                && candidate.uid == cached.uid
                && (!cached.strict
                    || (cached.addr.version() == candidate.addr.version()
                        && (cached.addr.is_unspecified() || cached.addr == candidate.addr)))
        })
    }

    fn check_conflict(&self, candidate: &TcpPortBinding) -> Result<(), SystemError> {
        // Mirrors inet_csk_get_port's cached admission. Current member options
        // and cached bucket admission are intentionally separate state.
        if (self.fast_reuseaddr && candidate.reuseaddr && !candidate.listening)
            || self.fast_reuseport_matches(candidate)
        {
            return Ok(());
        }
        for other in &self.members {
            if candidate.id == other.id || !tcp_addrs_conflict(candidate.addr, other.addr) {
                continue;
            }
            let reuseaddr_ok = candidate.reuseaddr && other.reuseaddr && !other.listening;
            let reuseport_ok = candidate.reuseport && other.reuseport && candidate.uid == other.uid;
            if !reuseaddr_ok && !reuseport_ok {
                return Err(SystemError::EADDRINUSE);
            }
        }
        Ok(())
    }

    fn update_fastreuse(&mut self, candidate: &TcpPortBinding) {
        let reuseaddr = candidate.reuseaddr && !candidate.listening;
        if self.members.is_empty() {
            self.fast_reuseaddr = reuseaddr;
            self.fast_reuseport = candidate.reuseport.then_some(FastReusePort {
                uid: candidate.uid,
                addr: candidate.addr,
                strict: false,
            });
        } else {
            if !reuseaddr {
                self.fast_reuseaddr = false;
            }
            if !candidate.reuseport {
                self.fast_reuseport = None;
            } else if !self.fast_reuseport_matches(candidate) {
                self.fast_reuseport = Some(FastReusePort {
                    uid: candidate.uid,
                    addr: candidate.addr,
                    strict: true,
                });
            }
        }
    }
}

/// TCP currently exposes default dual-stack IPv6 sockets. A native IPv6
/// wildcard therefore reserves IPv4 addresses too; concrete native IPv6
/// addresses remain disjoint. Keep UDP's existing policy separate.
fn tcp_addrs_conflict(a: IpAddress, b: IpAddress) -> bool {
    addrs_conflict(a, b)
        || (a.version() == smoltcp::wire::IpVersion::Ipv6 && a.is_unspecified())
        || (b.version() == smoltcp::wire::IpVersion::Ipv6 && b.is_unspecified())
}

#[inline]
fn addrs_conflict(a: IpAddress, b: IpAddress) -> bool {
    if a.version() != b.version() {
        return false;
    }
    if a.is_unspecified() || b.is_unspecified() {
        return true;
    }
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORT: u16 = 43001;
    fn loopback() -> IpAddress {
        IpAddress::v4(127, 0, 0, 1)
    }
    fn bind(
        pm: &PortManager,
        addr: IpAddress,
        ra: bool,
        rp: bool,
        uid: u32,
    ) -> Result<TcpBindId, SystemError> {
        let id = TcpBindId::new();
        pm.bind_tcp_port(PORT, addr, addr.version(), ra, rp, uid, id)?;
        Ok(id)
    }

    // The syscall layer normalizes mapped IPv6 addresses before admission.
    fn bind_family(pm: &PortManager, family: smoltcp::wire::IpVersion) -> TcpBindId {
        let id = TcpBindId::new();
        pm.bind_tcp_port(PORT, loopback(), family, false, true, 1000, id)
            .unwrap();
        id
    }

    #[test]
    fn ipv4_beats_mapped_ipv6_in_both_listen_orders() {
        use smoltcp::wire::IpVersion::{Ipv4, Ipv6};
        for ipv6_first in [false, true] {
            let pm = PortManager::default();
            let a = bind_family(&pm, Ipv4);
            let b = bind_family(&pm, Ipv6);
            for id in if ipv6_first { [b, a] } else { [a, b] } {
                pm.listen_tcp_port(PORT, id).unwrap();
            }
            {
                let table = pm.tcp_port_table.lock();
                assert!(table[&PORT].listener_is_selected(a.get()));
                assert!(!table[&PORT].listener_is_selected(b.get()));
            }
            pm.stop_tcp_listen(PORT, a);
            assert!(pm.tcp_port_table.lock()[&PORT].listener_is_selected(b.get()));
            pm.listen_tcp_port(PORT, a).unwrap();
            assert!(!pm.tcp_port_table.lock()[&PORT].listener_is_selected(b.get()));
        }
    }

    #[test]
    fn mapped_group_survives_ipv4_priority_and_member_close() {
        use smoltcp::wire::IpVersion::{Ipv4, Ipv6};
        let pm = PortManager::default();
        let a = bind_family(&pm, Ipv6);
        let b = bind_family(&pm, Ipv6);
        let v4 = bind_family(&pm, Ipv4);
        for id in [a, v4, b] {
            pm.listen_tcp_port(PORT, id).unwrap();
        }
        pm.stop_tcp_listen(PORT, v4);
        {
            let table = pm.tcp_port_table.lock();
            assert!(table[&PORT].listener_is_selected(a.get()));
            assert!(table[&PORT].listener_is_selected(b.get()));
        }
        pm.unbind_tcp_port(PORT, a);
        assert!(pm.tcp_port_table.lock()[&PORT].listener_is_selected(b.get()));
        pm.listen_tcp_port(PORT, v4).unwrap();
        assert!(!pm.tcp_port_table.lock()[&PORT].listener_is_selected(b.get()));
    }

    #[test]
    fn reuseaddr_shares_bound_sockets_but_not_listeners() {
        let pm = PortManager::default();
        let a = bind(&pm, loopback(), true, false, 1000).unwrap();
        let b = bind(&pm, loopback(), true, false, 1000).unwrap();
        pm.listen_tcp_port(PORT, a).unwrap();
        assert_eq!(pm.listen_tcp_port(PORT, b), Err(SystemError::EADDRINUSE));
        pm.stop_tcp_listen(PORT, a);
        pm.listen_tcp_port(PORT, b).unwrap();
    }

    #[test]
    fn reuseport_requires_the_same_owner() {
        let pm = PortManager::default();
        let a = bind(&pm, loopback(), false, true, 1000).unwrap();
        pm.listen_tcp_port(PORT, a).unwrap();
        assert_eq!(
            bind(&pm, loopback(), false, true, 1001),
            Err(SystemError::EADDRINUSE)
        );
        let b = bind(&pm, loopback(), false, true, 1000).unwrap();
        pm.listen_tcp_port(PORT, b).unwrap();
    }

    #[test]
    fn removing_one_binding_preserves_the_other() {
        let pm = PortManager::default();
        let a = bind(&pm, loopback(), false, true, 1000).unwrap();
        let b = bind(&pm, loopback(), false, true, 1000).unwrap();
        pm.unbind_tcp_port(PORT, a);
        assert_eq!(
            bind(&pm, loopback(), false, false, 1000),
            Err(SystemError::EADDRINUSE)
        );
        pm.unbind_tcp_port(PORT, a); // stale release must not remove a different member
        pm.unbind_tcp_port(PORT, b);
        assert!(bind(&pm, loopback(), false, false, 1000).is_ok());
    }

    #[test]
    fn enabling_reuseport_after_bind_is_visible_to_admission() {
        let pm = PortManager::default();
        let a = bind(&pm, loopback(), false, false, 1000).unwrap();
        pm.update_tcp_options(PORT, a, None, Some(true)).unwrap();
        pm.listen_tcp_port(PORT, a).unwrap();
        let b = bind(&pm, loopback(), false, true, 1000).unwrap();
        pm.listen_tcp_port(PORT, b).unwrap();
    }

    #[test]
    fn disabling_before_listen_revalidates_port_cache() {
        let pm = PortManager::default();
        let a = bind(&pm, loopback(), false, true, 1000).unwrap();
        pm.update_tcp_options(PORT, a, None, Some(false)).unwrap();
        pm.listen_tcp_port(PORT, a).unwrap();
        assert_eq!(
            bind(&pm, loopback(), false, true, 1000),
            Err(SystemError::EADDRINUSE)
        );
    }

    #[test]
    fn disabling_after_listen_keeps_linux_fastreuse_admission() {
        let pm = PortManager::default();
        let a = bind(&pm, loopback(), false, true, 1000).unwrap();
        pm.listen_tcp_port(PORT, a).unwrap();
        pm.update_tcp_options(PORT, a, None, Some(false)).unwrap();
        let b = bind(&pm, loopback(), false, true, 1000).unwrap();
        pm.listen_tcp_port(PORT, b).unwrap();
    }

    #[test]
    fn wildcard_conflicts_across_local_addresses() {
        let pm = PortManager::default();
        let a = bind(&pm, IpAddress::v4(0, 0, 0, 0), true, false, 1000).unwrap();
        let b = bind(&pm, loopback(), true, false, 1000).unwrap();
        pm.listen_tcp_port(PORT, a).unwrap();
        assert_eq!(pm.listen_tcp_port(PORT, b), Err(SystemError::EADDRINUSE));
    }

    #[test]
    fn different_addresses_and_native_families_do_not_conflict() {
        let pm = PortManager::default();
        bind(&pm, loopback(), false, false, 1000).unwrap();
        bind(&pm, IpAddress::v4(127, 0, 0, 2), false, false, 1001).unwrap();
        bind(
            &pm,
            IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1),
            false,
            false,
            1001,
        )
        .unwrap();
    }

    #[test]
    fn disabling_a_group_member_does_not_remove_its_membership() {
        let pm = PortManager::default();
        let a = bind(&pm, loopback(), false, true, 1000).unwrap();
        pm.listen_tcp_port(PORT, a).unwrap();
        let b = bind(&pm, loopback(), false, true, 1000).unwrap();
        pm.listen_tcp_port(PORT, b).unwrap();
        pm.update_tcp_options(PORT, a, None, Some(false)).unwrap();
        let table = pm.tcp_port_table.lock();
        assert!(table[&PORT].listener_is_selected(a.get()));
        assert!(table[&PORT].listener_is_selected(b.get()));
    }

    #[test]
    fn newest_non_reuseport_listener_takes_precedence_over_its_old_group() {
        let pm = PortManager::default();
        let a = bind(&pm, loopback(), false, true, 1000).unwrap();
        pm.listen_tcp_port(PORT, a).unwrap();
        let b = bind(&pm, loopback(), false, true, 1000).unwrap();
        pm.listen_tcp_port(PORT, b).unwrap();
        pm.update_tcp_options(PORT, b, None, Some(false)).unwrap();
        {
            let table = pm.tcp_port_table.lock();
            assert!(!table[&PORT].listener_is_selected(a.get()));
            assert!(table[&PORT].listener_is_selected(b.get()));
        }
        pm.unbind_tcp_port(PORT, b);
        assert!(pm.tcp_port_table.lock()[&PORT].listener_is_selected(a.get()));
    }

    #[test]
    fn disabled_singleton_does_not_join_a_new_group() {
        let pm = PortManager::default();
        let a = bind(&pm, loopback(), false, true, 1000).unwrap();
        pm.listen_tcp_port(PORT, a).unwrap();
        pm.update_tcp_options(PORT, a, None, Some(false)).unwrap();
        let b = bind(&pm, loopback(), false, true, 1000).unwrap();
        pm.listen_tcp_port(PORT, b).unwrap();
        let table = pm.tcp_port_table.lock();
        assert!(!table[&PORT].listener_is_selected(a.get()));
        assert!(table[&PORT].listener_is_selected(b.get()));
    }

    #[test]
    fn strict_cache_does_not_allow_a_wildcard_to_bypass_an_exact_owner() {
        let pm = PortManager::default();
        bind(&pm, loopback(), false, false, 1000).unwrap();
        bind(&pm, IpAddress::v4(127, 0, 0, 2), false, true, 1000).unwrap();
        assert_eq!(
            bind(&pm, IpAddress::v4(0, 0, 0, 0), false, true, 1000),
            Err(SystemError::EADDRINUSE)
        );
    }

    #[test]
    fn ipv6_reuseport_listeners_are_looked_up_in_insertion_order() {
        let pm = PortManager::default();
        let addr = IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1);
        let a = bind(&pm, addr, false, true, 1000).unwrap();
        pm.listen_tcp_port(PORT, a).unwrap();
        let b = bind(&pm, addr, false, true, 1000).unwrap();
        pm.listen_tcp_port(PORT, b).unwrap();
        pm.update_tcp_options(PORT, a, None, Some(false)).unwrap();
        let table = pm.tcp_port_table.lock();
        assert!(table[&PORT].listener_is_selected(a.get()));
        assert!(!table[&PORT].listener_is_selected(b.get()));
    }

    #[test]
    fn group_join_uses_listen_order_instead_of_bind_order() {
        let pm = PortManager::default();
        let a = bind(&pm, loopback(), false, true, 1000).unwrap();
        let b = bind(&pm, loopback(), false, true, 1000).unwrap();
        pm.listen_tcp_port(PORT, b).unwrap();
        pm.update_tcp_options(PORT, b, None, Some(false)).unwrap();
        pm.listen_tcp_port(PORT, a).unwrap();
        pm.update_tcp_options(PORT, b, None, Some(true)).unwrap();
        let c = bind(&pm, loopback(), false, true, 1000).unwrap();
        pm.listen_tcp_port(PORT, c).unwrap();
        let table = pm.tcp_port_table.lock();
        assert!(table[&PORT].listener_is_selected(a.get()));
        assert!(!table[&PORT].listener_is_selected(b.get()));
        assert!(table[&PORT].listener_is_selected(c.get()));
    }

    #[test]
    fn default_ipv6_wildcard_owns_the_ipv4_port_too() {
        let any6 = IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 0);
        for (first, second) in [(any6, loopback()), (loopback(), any6)] {
            let pm = PortManager::default();
            bind(&pm, first, false, false, 1000).unwrap();
            assert_eq!(
                bind(&pm, second, false, false, 1000),
                Err(SystemError::EADDRINUSE)
            );
        }
    }

    #[test]
    fn dual_stack_and_ipv4_reuseport_members_have_separate_address_groups() {
        let pm = PortManager::default();
        let a = bind(
            &pm,
            IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 0),
            false,
            true,
            1000,
        )
        .unwrap();
        pm.listen_tcp_port(PORT, a).unwrap();
        let b = bind(&pm, IpAddress::v4(0, 0, 0, 0), false, true, 1000).unwrap();
        pm.listen_tcp_port(PORT, b).unwrap();
        let table = pm.tcp_port_table.lock();
        assert!(table[&PORT].listener_is_selected(a.get()));
        assert!(table[&PORT].listener_is_selected(b.get()));
        assert_ne!(table[&PORT].members[0].group, table[&PORT].members[1].group);
    }
}
