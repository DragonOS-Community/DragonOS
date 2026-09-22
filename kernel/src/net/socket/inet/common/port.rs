use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use hashbrown::{HashMap, HashSet};
use smoltcp::socket::tcp::{LifecycleObserver, State};
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, IpVersion};
use system_error::SystemError;

use super::device_binding::SocketDeviceBinding;
use crate::{arch::rand::rand, libs::mutex::Mutex, process::ProcessManager};

/// A normalized TCP receive domain. Mapped IPv6 addresses are normalized to
/// IPv4 before construction; only the IPv6 wildcard may cover both families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpBindDomain {
    pub addr: IpAddress,
    pub ip_version: Option<IpVersion>,
}

impl TcpBindDomain {
    pub fn new(addr: IpAddress, v6_only: bool) -> Self {
        let ip_version = if addr.version() == IpVersion::Ipv6 && addr.is_unspecified() && !v6_only {
            None
        } else {
            Some(addr.version())
        };
        Self { addr, ip_version }
    }

    pub fn matches(&self, addr: IpAddress) -> bool {
        self.ip_version
            .is_none_or(|version| version == addr.version())
            && (self.addr.is_unspecified() || self.addr == addr)
    }

    fn overlaps(&self, other: Self) -> bool {
        if let (Some(a), Some(b)) = (self.ip_version, other.ip_version) {
            if a != b {
                return false;
            }
        }
        self.addr.is_unspecified() || other.addr.is_unspecified() || self.addr == other.addr
    }

    pub fn listen_endpoint(&self, port: u16) -> IpListenEndpoint {
        if self.addr.is_unspecified() {
            port.into()
        } else {
            IpEndpoint::new(self.addr, port).into()
        }
    }
}

#[derive(Debug)]
struct Binding {
    identity: Weak<Identity>,
    observer: Weak<PortObserver>,
    observer_generation: u64,
    port: u16,
    domain: Option<TcpBindDomain>,
    device: Arc<SocketDeviceBinding>,
    reuse: bool,
    fd_bound: bool,
    port_locked: bool,
    listening: bool,
    parent: Option<u64>,
    protocol: Option<State>,
    tuple: Option<(IpEndpoint, IpEndpoint)>,
    protocol_device: u32,
    time_wait_reuse: Option<bool>,
}

#[derive(Debug, Default)]
struct Bindings {
    owners: HashMap<u64, Binding>,
    ports: HashMap<u16, HashSet<u64>>,
    tuples: HashMap<(IpEndpoint, IpEndpoint), Vec<u64>>,
}

impl Bindings {
    /// Maintain the lookup index under the same lock as the authoritative
    /// identity. A tuple bucket normally has one entry; distinct bound devices
    /// may legitimately own the same endpoints.
    fn set_tuple(&mut self, id: u64, tuple: Option<(IpEndpoint, IpEndpoint)>) {
        let old = self.owners[&id].tuple;
        if old == tuple {
            return;
        }
        if let Some(old) = old {
            let bucket = self.tuples.get_mut(&old).unwrap();
            bucket.retain(|owner| *owner != id);
            if bucket.is_empty() {
                self.tuples.remove(&old);
            }
        }
        if let Some(tuple) = tuple {
            self.tuples.entry(tuple).or_default().push(id);
        }
        self.owners.get_mut(&id).unwrap().tuple = tuple;
    }
}

/// Namespace-wide binding and tuple identities. This table never takes socket
/// locks and does not own a namespace, interface, or identity handle.
#[derive(Debug)]
pub struct PortManager {
    bindings: Mutex<Bindings>,
    next_id: AtomicU64,
    next_ephemeral: AtomicU16,
}

impl Default for PortManager {
    fn default() -> Self {
        Self {
            bindings: Mutex::new(Bindings::default()),
            next_id: AtomicU64::new(1),
            next_ephemeral: AtomicU16::new(0),
        }
    }
}

pub const DEFAULT_LOCAL_PORT_RANGE: u32 = (32768u32 << 16) | 60999u32;

#[derive(Debug)]
struct Identity {
    manager: Arc<PortManager>,
    id: u64,
}

impl Identity {
    fn observer(self: &Arc<Self>) -> Arc<dyn LifecycleObserver> {
        let mut table = self.manager.bindings.lock();
        let binding = table.owners.get_mut(&self.id).unwrap();
        if let Some(observer) = binding.observer.upgrade() {
            return observer;
        }
        binding.observer_generation = binding.observer_generation.wrapping_add(1);
        let observer = Arc::new(PortObserver {
            identity: self.clone(),
            generation: binding.observer_generation,
        });
        binding.observer = Arc::downgrade(&observer);
        observer
    }
}

impl Drop for Identity {
    fn drop(&mut self) {
        let mut table = self.manager.bindings.lock();
        table.set_tuple(self.id, None);
        if let Some(binding) = table.owners.remove(&self.id) {
            if let Some(bucket) = table.ports.get_mut(&binding.port) {
                bucket.remove(&self.id);
                if bucket.is_empty() {
                    table.ports.remove(&binding.port);
                }
            }
        }
    }
}

/// Socket option owner, including before bind. Cloning this handle does not
/// acquire another FD binding or protocol lifetime.
#[derive(Debug, Clone)]
pub struct TcpPortOwner(Arc<Identity>);

impl TcpPortOwner {
    pub fn device_binding(&self) -> Arc<SocketDeviceBinding> {
        self.0.manager.bindings.lock().owners[&self.0.id]
            .device
            .clone()
    }
    pub fn reuse_addr(&self) -> bool {
        self.0.manager.bindings.lock().owners[&self.0.id].reuse
    }

    pub fn set_reuse_addr(&self, reuse: bool) {
        self.0
            .manager
            .bindings
            .lock()
            .owners
            .get_mut(&self.0.id)
            .unwrap()
            .reuse = reuse;
    }

    pub fn reserve(
        &self,
        domain: TcpBindDomain,
        port: u16,
        range: (u16, u16),
    ) -> Result<TcpPortReservation, SystemError> {
        self.0
            .manager
            .reserve_owner(self, domain, port, range, None)
    }

    /// Implicit connect allocates by full tuple, unlike explicit bind. A
    /// same-tuple TIME_WAIT candidate still requires the protocol's safe-reuse
    /// transaction; a rejected candidate can be released and allocation retried.
    pub fn reserve_connect(
        &self,
        domain: TcpBindDomain,
        remote: IpEndpoint,
        range: (u16, u16),
    ) -> Result<TcpPortReservation, SystemError> {
        self.0
            .manager
            .reserve_owner(self, domain, 0, range, Some(remote))
    }
}

/// The unique FD-side binding reference. Protocol observers independently keep
/// the same identity alive after close; TIME_WAIT can outlive the original FD.
#[derive(Debug)]
pub struct TcpPortReservation {
    identity: Arc<Identity>,
    pub id: u64,
    pub port: u16,
    pub domain: TcpBindDomain,
    pub(crate) locked_bind_domain: Option<TcpBindDomain>,
}

impl Drop for TcpPortReservation {
    fn drop(&mut self) {
        let mut table = self.identity.manager.bindings.lock();
        let binding = table.owners.get_mut(&self.id).unwrap();
        binding.fd_bound = false;
        binding.listening = false;
    }
}

impl TcpPortReservation {
    /// A successful implicit source selection narrows a wildcard reservation
    /// without allocating a new port or repeating bind-time conflict checks.
    pub(crate) fn update_domain(&mut self, domain: TcpBindDomain) {
        let mut bindings = self.identity.manager.bindings.lock();
        bindings.owners.get_mut(&self.id).unwrap().domain = Some(domain);
        self.domain = domain;
    }

    pub fn owner(&self) -> TcpPortOwner {
        TcpPortOwner(self.identity.clone())
    }

    pub fn lifecycle_observer(&self) -> Arc<dyn LifecycleObserver> {
        self.identity.observer()
    }

    pub fn prepare_child(
        &self,
        device: Arc<SocketDeviceBinding>,
    ) -> Result<Arc<dyn LifecycleObserver>, SystemError> {
        let manager = &self.identity.manager;
        let owner = manager.new_owner(device);
        {
            let mut table = manager.bindings.lock();
            table
                .ports
                .entry(self.port)
                .or_default()
                .try_reserve(1)
                .map_err(|_| SystemError::ENOMEM)?;
            let binding = table.owners.get_mut(&owner.0.id).unwrap();
            binding.port = self.port;
            binding.parent = Some(self.id);
            table.ports.get_mut(&self.port).unwrap().insert(owner.0.id);
        }
        Ok(owner.0.observer())
    }

    pub fn promote_listener(&self) -> Result<ListenPromotion, SystemError> {
        let mut table = self.identity.manager.bindings.lock();
        let mine = &table.owners[&self.id];
        if let Some(ids) = table.ports.get(&self.port) {
            if ids.iter().any(|id| {
                *id != self.id
                    && bind_conflicts(
                        mine.domain.unwrap(),
                        mine.device.ifindex() as u32,
                        mine.reuse,
                        &table.owners[id],
                    )
            }) {
                return Err(SystemError::EADDRINUSE);
            }
        }
        let previous = table.owners.get_mut(&self.id).unwrap().listening;
        table.owners.get_mut(&self.id).unwrap().listening = true;
        Ok(ListenPromotion {
            identity: self.identity.clone(),
            previous,
            committed: false,
        })
    }
}

#[derive(Debug)]
pub struct ListenPromotion {
    identity: Arc<Identity>,
    previous: bool,
    committed: bool,
}

impl ListenPromotion {
    pub fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for ListenPromotion {
    fn drop(&mut self) {
        if !self.committed {
            self.identity
                .manager
                .bindings
                .lock()
                .owners
                .get_mut(&self.identity.id)
                .unwrap()
                .listening = self.previous;
        }
    }
}

fn devices_overlap(a: u32, b: u32) -> bool {
    a == 0 || b == 0 || a == b
}

fn bind_conflicts(domain: TcpBindDomain, device: u32, reuse: bool, existing: &Binding) -> bool {
    if existing.fd_bound
        && devices_overlap(device, existing.device.ifindex() as u32)
        && domain.overlaps(existing.domain.unwrap())
        && !(reuse && existing.reuse && !existing.listening)
    {
        return true;
    }
    if matches!(
        existing.protocol,
        Some(
            State::Established
                | State::FinWait1
                | State::FinWait2
                | State::Closing
                | State::CloseWait
                | State::LastAck
                | State::TimeWait
        )
    ) {
        if let Some((local, _)) = existing.tuple {
            let old_reuse = existing.time_wait_reuse.unwrap_or(existing.reuse);
            return devices_overlap(device, existing.protocol_device)
                && domain.matches(local.addr)
                && !(reuse && old_reuse);
        }
    }
    false
}

#[derive(Debug)]
struct PortObserver {
    identity: Arc<Identity>,
    generation: u64,
}

impl Drop for PortObserver {
    fn drop(&mut self) {
        let mut table = self.identity.manager.bindings.lock();
        if let Some(binding) = table.owners.get_mut(&self.identity.id) {
            if binding.observer_generation != self.generation {
                return;
            }
            binding.protocol = None;
            binding.time_wait_reuse = None;
            table.set_tuple(self.identity.id, None);
        }
    }
}

impl LifecycleObserver for PortObserver {
    fn identity(&self) -> u64 {
        self.identity.id
    }

    fn prepare_open(
        &self,
        local: IpEndpoint,
        remote: IpEndpoint,
        device: u32,
        replacing: Option<u64>,
    ) -> bool {
        let mut table = self.identity.manager.bindings.lock();
        if replacing == Some(self.identity.id) {
            return false;
        }
        if let Some(id) = replacing {
            let Some(old) = table.owners.get(&id) else {
                return false;
            };
            if old.protocol != Some(State::TimeWait)
                || old.tuple != Some((local, remote))
                || !devices_overlap(device, old.protocol_device)
            {
                return false;
            }
        }
        if table.tuples.get(&(local, remote)).is_some_and(|ids| {
            ids.iter().any(|id| {
                if *id == self.identity.id || Some(*id) == replacing {
                    return false;
                }
                let other = &table.owners[id];
                devices_overlap(device, other.protocol_device)
            })
        }) {
            return false;
        }
        let mine = table.owners.get_mut(&self.identity.id).unwrap();
        if mine.port != local.port || mine.tuple.is_some() {
            return false;
        }
        mine.protocol_device = device;
        mine.protocol = Some(if mine.parent.is_some() {
            State::SynReceived
        } else {
            State::SynSent
        });
        if mine.parent.is_some() {
            mine.domain = Some(TcpBindDomain::new(local.addr, true));
        }
        mine.time_wait_reuse = None;
        table.set_tuple(self.identity.id, Some((local, remote)));
        if let Some(old) = replacing {
            table.set_tuple(old, None);
        }
        true
    }

    fn on_state_change(
        &self,
        state: State,
        local: Option<IpEndpoint>,
        remote: Option<IpEndpoint>,
        device: u32,
    ) {
        let mut table = self.identity.manager.bindings.lock();
        let old = &table.owners[&self.identity.id];
        let previous = old.protocol;
        let inherited = if previous == Some(State::SynReceived)
            && matches!(state, State::Established | State::CloseWait)
        {
            old.parent.and_then(|id| {
                table
                    .owners
                    .get(&id)
                    .map(|parent| (parent.reuse, parent.port_locked))
            })
        } else {
            None
        };
        let mine = table.owners.get_mut(&self.identity.id).unwrap();
        if let Some((reuse, port_locked)) = inherited {
            mine.reuse = reuse;
            mine.port_locked = port_locked;
        }
        mine.protocol = Some(state);
        mine.protocol_device = device;
        // Linux tcp_done releases an automatically selected local port even
        // while the descriptor survives. Explicit nonzero bind owns it until
        // close; protocol/TIME_WAIT ownership remains independent either way.
        if !mine.port_locked
            && (state == State::TimeWait
                || (state == State::Closed
                    && previous.is_some_and(|previous| {
                        !matches!(previous, State::Closed | State::Listen)
                    })))
        {
            mine.fd_bound = false;
        }
        if matches!(state, State::Closed | State::Listen) {
            mine.time_wait_reuse = None;
            table.set_tuple(self.identity.id, None);
        } else {
            if state == State::TimeWait && mine.time_wait_reuse.is_none() {
                mine.time_wait_reuse = Some(mine.reuse);
            }
            if let (Some(local), Some(remote)) = (local, remote) {
                table.set_tuple(self.identity.id, Some((local, remote)));
            }
        }
    }
}

impl PortManager {
    pub fn new_owner(self: &Arc<Self>, device: Arc<SocketDeviceBinding>) -> TcpPortOwner {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let identity = Arc::new(Identity {
            manager: self.clone(),
            id,
        });
        self.bindings.lock().owners.insert(
            id,
            Binding {
                identity: Arc::downgrade(&identity),
                observer: Weak::new(),
                observer_generation: 0,
                port: 0,
                domain: None,
                device,
                reuse: false,
                fd_bound: false,
                port_locked: false,
                listening: false,
                parent: None,
                protocol: None,
                tuple: None,
                protocol_device: 0,
                time_wait_reuse: None,
            },
        );
        TcpPortOwner(identity)
    }

    pub fn claim_fd(&self, id: u64) -> Result<TcpPortReservation, SystemError> {
        let mut table = self.bindings.lock();
        let entry = table.owners.get_mut(&id).ok_or(SystemError::EINVAL)?;
        if entry.fd_bound
            || entry.parent.is_none()
            || !matches!(entry.protocol, Some(State::Established | State::CloseWait))
        {
            return Err(SystemError::EINVAL);
        }
        let identity = entry.identity.upgrade().ok_or(SystemError::EINVAL)?;
        entry.device = Arc::new(SocketDeviceBinding::from_ifindex(
            entry.protocol_device as usize,
        ));
        entry.fd_bound = true;
        Ok(TcpPortReservation {
            identity,
            id,
            port: entry.port,
            domain: entry.domain.unwrap(),
            locked_bind_domain: entry.port_locked.then_some(entry.domain.unwrap()),
        })
    }

    pub fn local_port_range() -> (u16, u16) {
        ProcessManager::current_netns().local_port_range()
    }

    pub fn set_local_port_range(min: u16, max: u16) -> Result<(), SystemError> {
        ProcessManager::current_netns().set_local_port_range(min, max)
    }

    fn reserve_owner(
        &self,
        owner: &TcpPortOwner,
        domain: TcpBindDomain,
        port: u16,
        range: (u16, u16),
        connect_remote: Option<IpEndpoint>,
    ) -> Result<TcpPortReservation, SystemError> {
        let manager = self;
        let (min, max) = range;
        let count = u32::from(max) - u32::from(min) + 1;
        let mut bindings = manager.bindings.lock();
        let mine = &bindings.owners[&owner.0.id];
        if mine.fd_bound || mine.tuple.is_some() {
            return Err(SystemError::EINVAL);
        }
        let reuse = mine.reuse;
        let device = mine.device.ifindex() as u32;
        let old_port = mine.port;
        let initial = manager.next_ephemeral.load(Ordering::Relaxed);
        let mut candidate = if port != 0 {
            port
        } else if initial >= min && initial <= max {
            initial
        } else {
            min + (rand() % count as usize) as u16
        };
        for _ in 0..if port == 0 { count } else { 1 } {
            if !bindings.ports.get(&candidate).is_some_and(|ids| {
                ids.iter().any(|id| {
                    if *id == owner.0.id {
                        return false;
                    }
                    let other = &bindings.owners[id];
                    if let Some(remote) = connect_remote {
                        if other.fd_bound
                            && devices_overlap(device, other.device.ifindex() as u32)
                            && domain.overlaps(other.domain.unwrap())
                            && (other.listening || other.tuple.is_none())
                        {
                            return true;
                        }
                        if let Some((local, peer)) = other.tuple {
                            return devices_overlap(device, other.protocol_device)
                                && domain.matches(local.addr)
                                && peer == remote
                                && other.protocol != Some(State::TimeWait);
                        }
                        false
                    } else {
                        bind_conflicts(domain, device, reuse, other)
                    }
                })
            }) {
                let bucket = bindings.ports.entry(candidate).or_default();
                bucket.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
                let id = owner.0.id;
                bucket.insert(id);
                if old_port != 0 && old_port != candidate {
                    if let Some(previous) = bindings.ports.get_mut(&old_port) {
                        previous.remove(&id);
                        if previous.is_empty() {
                            bindings.ports.remove(&old_port);
                        }
                    }
                }
                let binding = bindings.owners.get_mut(&id).unwrap();
                binding.port = candidate;
                binding.domain = Some(domain);
                binding.fd_bound = true;
                binding.port_locked = port != 0 && connect_remote.is_none();
                if port == 0 {
                    manager.next_ephemeral.store(
                        if candidate == max { min } else { candidate + 1 },
                        Ordering::Relaxed,
                    );
                }
                drop(bindings);
                return Ok(TcpPortReservation {
                    identity: owner.0.clone(),
                    id,
                    port: candidate,
                    domain,
                    locked_bind_domain: (port != 0 && connect_remote.is_none()).then_some(domain),
                });
            }
            if port != 0 {
                break;
            }
            candidate = if candidate == max { min } else { candidate + 1 };
        }
        Err(if connect_remote.is_some() {
            SystemError::EADDRNOTAVAIL
        } else {
            SystemError::EADDRINUSE
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> Arc<PortManager> {
        Arc::new(PortManager::default())
    }
    fn owner(manager: &Arc<PortManager>, reuse: bool) -> TcpPortOwner {
        let owner = manager.new_owner(Arc::new(SocketDeviceBinding::default()));
        owner.set_reuse_addr(reuse);
        owner
    }
    fn local(port: u16) -> IpEndpoint {
        IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), port)
    }
    fn domain() -> TcpBindDomain {
        TcpBindDomain::new(local(0).addr, true)
    }
    fn reserve(owner: &TcpPortOwner) -> Result<TcpPortReservation, SystemError> {
        owner.reserve(domain(), 40000, (40000, 40010))
    }

    #[test]
    fn reused_bind_has_atomic_single_listener_promotion() {
        let manager = manager();
        let a = reserve(&owner(&manager, true)).unwrap();
        let b = reserve(&owner(&manager, true)).unwrap();
        let promotion = a.promote_listener().unwrap();
        assert!(matches!(b.promote_listener(), Err(SystemError::EADDRINUSE)));
        drop(promotion);
        b.promote_listener().unwrap().commit();
        assert!(matches!(a.promote_listener(), Err(SystemError::EADDRINUSE)));
    }

    #[test]
    fn child_inherits_at_handshake_not_accept() {
        let manager = manager();
        let parent = owner(&manager, false);
        let listener = reserve(&parent).unwrap();
        listener.promote_listener().unwrap().commit();
        let child = listener
            .prepare_child(Arc::new(SocketDeviceBinding::default()))
            .unwrap();
        assert!(child.prepare_open(local(40000), local(41000), 2, None));
        parent.set_reuse_addr(true);
        child.on_state_change(
            State::Established,
            Some(local(40000)),
            Some(local(41000)),
            2,
        );
        parent.set_reuse_addr(false);
        let accepted = manager.claim_fd(child.identity()).unwrap();
        assert!(accepted.owner().reuse_addr());
        assert_eq!(accepted.owner().device_binding().ifindex(), 2);
        drop(listener);
        assert!(matches!(
            reserve(&owner(&manager, false)),
            Err(SystemError::EADDRINUSE)
        ));
        assert!(reserve(&owner(&manager, true)).is_ok());
    }

    #[test]
    fn protocol_close_preserves_fd_bind_and_protocol_drop_preserves_options() {
        let manager = manager();
        let original = owner(&manager, false);
        let fd = reserve(&original).unwrap();
        let protocol = fd.lifecycle_observer();
        assert!(protocol.prepare_open(local(40000), local(41000), 0, None));
        protocol.on_state_change(State::Closed, None, None, 0);
        drop(protocol);
        assert!(matches!(
            reserve(&owner(&manager, false)),
            Err(SystemError::EADDRINUSE)
        ));
        drop(fd);
        assert!(reserve(&owner(&manager, false)).is_ok());
        original.set_reuse_addr(true);
        assert!(original.reuse_addr());
    }

    #[test]
    fn time_wait_reuse_is_frozen_and_replacement_drop_is_by_identity() {
        let manager = manager();
        let original = owner(&manager, false);
        let fd = reserve(&original).unwrap();
        let old = fd.lifecycle_observer();
        assert!(old.prepare_open(local(40000), local(41000), 0, None));
        old.on_state_change(State::TimeWait, Some(local(40000)), Some(local(41000)), 0);
        original.set_reuse_addr(true);
        drop(fd);
        assert!(matches!(
            reserve(&owner(&manager, true)),
            Err(SystemError::EADDRINUSE)
        ));
        let next_owner = owner(&manager, false);
        let next = next_owner
            .reserve_connect(domain(), local(41000), (40000, 40000))
            .unwrap();
        let next_protocol = next.lifecycle_observer();
        assert!(next_protocol.prepare_open(local(40000), local(41000), 0, Some(old.identity())));
        drop(old);
        assert!(matches!(
            owner(&manager, false).reserve_connect(domain(), local(41000), (40000, 40000)),
            Err(SystemError::EADDRNOTAVAIL)
        ));
    }

    #[test]
    fn cross_interface_tuple_conflict_uses_device_overlap() {
        let manager = manager();
        let first = reserve(&owner(&manager, true)).unwrap();
        let second = reserve(&owner(&manager, true)).unwrap();
        let a = first.lifecycle_observer();
        let b = second.lifecycle_observer();
        assert!(a.prepare_open(local(40000), local(41000), 2, None));
        assert!(!b.prepare_open(local(40000), local(41000), 0, None));
        assert!(b.prepare_open(local(40000), local(41000), 3, None));
    }

    #[test]
    fn one_identity_has_one_protocol_observer() {
        let manager = manager();
        let fd = reserve(&owner(&manager, false)).unwrap();
        let a = fd.lifecycle_observer();
        let b = fd.lifecycle_observer();
        assert!(a.prepare_open(local(40000), local(41000), 0, None));
        drop(a);
        assert!(matches!(
            owner(&manager, false).reserve_connect(domain(), local(41000), (40000, 40000)),
            Err(SystemError::EADDRNOTAVAIL)
        ));
        drop(b);
        drop(fd);
        assert!(reserve(&owner(&manager, false)).is_ok());
    }

    #[test]
    fn tuple_index_tracks_device_domains_and_observer_release() {
        let manager = manager();
        let first = reserve(&owner(&manager, true)).unwrap();
        let second = reserve(&owner(&manager, true)).unwrap();
        let a = first.lifecycle_observer();
        let b = second.lifecycle_observer();
        let tuple = (local(40000), local(41000));
        assert!(a.prepare_open(tuple.0, tuple.1, 2, None));
        assert!(b.prepare_open(tuple.0, tuple.1, 3, None));
        assert_eq!(manager.bindings.lock().tuples[&tuple].len(), 2);
        a.on_state_change(State::Closed, None, None, 2);
        assert_eq!(manager.bindings.lock().tuples[&tuple], [b.identity()]);
        drop(b);
        assert!(!manager.bindings.lock().tuples.contains_key(&tuple));
        drop(a);
        drop(first);
        drop(second);
        let table = manager.bindings.lock();
        assert!(table.ports.is_empty());
        assert!(table.owners.is_empty());
    }

    #[test]
    fn retired_time_wait_cannot_remove_replacement_tuple_index() {
        let manager = manager();
        let first = reserve(&owner(&manager, true)).unwrap();
        let second = reserve(&owner(&manager, true)).unwrap();
        let old = first.lifecycle_observer();
        let new = second.lifecycle_observer();
        let tuple = (local(40000), local(41000));
        assert!(old.prepare_open(tuple.0, tuple.1, 0, None));
        old.on_state_change(State::TimeWait, Some(tuple.0), Some(tuple.1), 0);
        assert!(new.prepare_open(tuple.0, tuple.1, 0, Some(old.identity())));
        old.on_state_change(State::Closed, None, None, 0);
        drop(old);
        drop(first);
        assert_eq!(manager.bindings.lock().tuples[&tuple], [new.identity()]);
        drop(new);
        drop(second);
        let table = manager.bindings.lock();
        assert!(table.tuples.is_empty());
        assert!(table.ports.is_empty());
    }
}
