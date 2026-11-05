//! Alloc-backed TIME-WAIT storage, excluded from active socket polling.
use super::*;
use crate::phy::PacketMeta;
use crate::socket::tcp::{
    self, LifecycleObserver, State, TimeWaitAction, TimeWaitReuse, TimeWaitState,
};
use crate::time::Instant;
use crate::wire::{IpRepr, TcpRepr};
use alloc::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    local: IpEndpoint,
    remote: IpEndpoint,
    device: u32,
}
impl Key {
    fn of(state: &TimeWaitState) -> Self {
        Self {
            local: state.tuple.local,
            remote: state.tuple.remote,
            device: state.device,
        }
    }
}
#[derive(Debug)]
struct Entry {
    state: TimeWaitState,
    observer: Option<Shared<dyn LifecycleObserver>>,
}
impl Entry {
    fn retire(self) {
        if let Some(observer) = &self.observer {
            observer.on_state_change(
                State::Closed,
                Some(self.state.tuple.local),
                Some(self.state.tuple.remote),
                self.state.device,
            );
        }
    }
}
#[derive(Debug, Default)]
pub(super) struct TimeWaitTable {
    entries: BTreeMap<Key, Entry>,
    deadlines: BTreeSet<(Instant, Key)>,
}
impl TimeWaitTable {
    fn remove(&mut self, key: Key) -> Option<Entry> {
        let entry = self.entries.remove(&key)?;
        self.deadlines.remove(&(entry.state.expires, key));
        Some(entry)
    }
    fn refresh(&mut self, key: Key, state: TimeWaitState) {
        let entry = self.entries.get_mut(&key).unwrap();
        self.deadlines.remove(&(entry.state.expires, key));
        self.deadlines.insert((state.expires, key));
        entry.state = state;
    }
    fn find(&self, local: IpEndpoint, remote: IpEndpoint, device: u32) -> Option<Key> {
        let exact = Key {
            local,
            remote,
            device,
        };
        if self.entries.contains_key(&exact) {
            return Some(exact);
        }
        let any = Key { device: 0, ..exact };
        self.entries.contains_key(&any).then_some(any)
    }
}

impl<'storage, 'a: 'storage> SocketSet<'storage, 'a> {
    pub fn has_tcp_time_wait(&self) -> bool {
        !self.time_wait.entries.is_empty()
    }
    /// Detach a TIME-WAIT socket after the integration has relinquished every
    /// handle user. Allocation follows SocketSet::add's alloc failure policy.
    /// False means the socket is not yet TIME-WAIT (or still owes a final ACK).
    pub fn detach_tcp_time_wait(&mut self, handle: SocketHandle) -> bool {
        let socket = self.get_mut::<tcp::Socket>(handle);
        if socket.state() != State::TimeWait
            || socket.time_wait.is_none()
            || socket.ack_to_transmit()
        {
            return false;
        }
        let state = socket.time_wait.as_ref().unwrap().clone();
        let key = Key::of(&state);
        if self.time_wait.entries.contains_key(&key) {
            return false;
        }
        // All allocation completes before removing the full socket/observer.
        self.time_wait.deadlines.insert((state.expires, key));
        self.time_wait.entries.insert(
            key,
            Entry {
                state,
                observer: None,
            },
        );
        let observer = self
            .get_mut::<tcp::Socket>(handle)
            .lifecycle_observer
            .take();
        self.time_wait.entries.get_mut(&key).unwrap().observer = observer;
        self.remove(handle);
        true
    }

    pub(crate) fn expire_tcp_time_wait(&mut self, now: Instant) {
        // Bound work per poll; poll_at keeps returning the oldest deadline.
        for _ in 0..64 {
            let Some(&(deadline, key)) = self.time_wait.deadlines.first() else {
                break;
            };
            if deadline > now {
                break;
            }
            self.time_wait.remove(key).unwrap().retire();
        }
    }

    pub(crate) fn tcp_time_wait_poll_at(&self) -> Option<Instant> {
        self.time_wait.deadlines.first().map(|(time, _)| *time)
    }

    /// Atomically validate/open an outgoing TCP connection, including existing
    /// TIME-WAIT protection. The caller supplies a concrete selected source.
    pub fn connect_tcp(
        &mut self,
        handle: SocketHandle,
        cx: &mut crate::iface::Context,
        remote: IpEndpoint,
        local: IpEndpoint,
        mode: TimeWaitReuse,
    ) -> Result<(), tcp::ConnectError> {
        let socket = self.get::<tcp::Socket>(handle);
        if socket.is_open() {
            return Err(tcp::ConnectError::InvalidState);
        }
        if local.port == 0
            || remote.port == 0
            || local.addr.is_unspecified()
            || remote.addr.is_unspecified()
            || local.addr.version() != remote.addr.version()
        {
            return Err(tcp::ConnectError::Unaddressable);
        }
        let device = socket.lifecycle_device();
        let mut matching = self.iter().filter_map(|(h, s)| {
            let s = tcp::Socket::downcast(s)?;
            (h != handle
                && s.local_endpoint() == Some(local)
                && s.remote_endpoint() == Some(remote)
                && (device == 0 || s.lifecycle_device() == 0 || s.lifecycle_device() == device))
                .then_some(h)
        });
        let full = matching.next();
        if matching.next().is_some() {
            return Err(tcp::ConnectError::AddressInUse);
        }
        drop(matching);
        let mut reuse = None;
        let mut identity = None;
        if let Some(h) = full {
            let old = self.get::<tcp::Socket>(h);
            if old.state() != State::TimeWait {
                return Err(tcp::ConnectError::AddressInUse);
            }
            let state = old
                .time_wait
                .as_ref()
                .ok_or(tcp::ConnectError::AddressInUse)?;
            if state.expires > cx.now() {
                if !state.active_reuse(cx.now(), mode) {
                    return Err(tcp::ConnectError::AddressInUse);
                }
                reuse = Some(state.clone());
            }
            identity = old.lifecycle_identity();
        }
        // Unbound sockets overlap every device domain, not just device zero.
        let key = if device == 0 {
            let first = Key {
                local,
                remote,
                device: 0,
            };
            let last = Key {
                device: u32::MAX,
                ..first
            };
            let mut matching = self.time_wait.entries.range(first..=last);
            let key = matching.next().map(|(k, _)| *k);
            if matching.next().is_some() {
                return Err(tcp::ConnectError::AddressInUse);
            }
            key
        } else {
            self.time_wait.find(local, remote, device)
        };
        if full.is_some() && key.is_some() {
            return Err(tcp::ConnectError::AddressInUse);
        }
        if let Some(key) = key {
            let old = self.time_wait.entries.get(&key).unwrap();
            if old.state.expires > cx.now() {
                if !old.state.active_reuse(cx.now(), mode) {
                    return Err(tcp::ConnectError::AddressInUse);
                }
                reuse = Some(old.state.clone());
            }
            identity = old.observer.as_ref().map(|o| o.identity());
        }
        let socket = self.get::<tcp::Socket>(handle);
        if reuse.is_some() && !socket.timestamp_configured() {
            return Err(tcp::ConnectError::AddressInUse);
        }
        if !socket.prepare_open(local, remote, identity) {
            return Err(tcp::ConnectError::AddressInUse);
        }
        // No ordinary failure is possible after the identity preparation.
        self.get_mut::<tcp::Socket>(handle)
            .open_connection(cx, local, remote, reuse.as_ref());
        if let Some(h) = full {
            self.get_mut::<tcp::Socket>(h).retire_time_wait();
        }
        if let Some(key) = key {
            self.time_wait.remove(key).unwrap().retire();
        }
        Ok(())
    }

    pub(crate) fn process_tcp_sockets(
        &mut self,
        cx: &mut crate::iface::Context,
        meta: PacketMeta,
        ip: &IpRepr,
        tcp: &TcpRepr<'_>,
    ) -> Option<Option<(PacketMeta, IpRepr, TcpRepr<'static>)>> {
        let full = self.iter().find_map(|(h, s)| {
            let s = tcp::Socket::downcast(s)?;
            (!s.is_listening() && s.accepts_ingress(meta) && s.accepts(cx, ip, tcp)).then_some(h)
        });
        if let Some(h) = full {
            if self.get::<tcp::Socket>(h).state() != State::TimeWait {
                let s = self.get_mut::<tcp::Socket>(h);
                let tx = s.egress_meta();
                return Some(
                    s.process_with_meta(cx, meta, ip, tcp)
                        .map(|(ip, tcp)| (tx, ip, tcp)),
                );
            }
        }
        #[cfg(feature = "packetmeta-id")]
        let device = meta.id;
        #[cfg(not(feature = "packetmeta-id"))]
        let device = 0;
        let key = self.time_wait.find(
            IpEndpoint::new(ip.dst_addr(), tcp.dst_port),
            IpEndpoint::new(ip.src_addr(), tcp.src_port),
            device,
        );
        let old = if let Some(h) = full {
            let s = self.get_mut::<tcp::Socket>(h);
            s.ensure_time_wait(cx.now());
            Some((
                s.time_wait.as_ref().unwrap().clone(),
                s.lifecycle_identity(),
            ))
        } else {
            key.map(|k| {
                let e = self.time_wait.entries.get(&k).unwrap();
                (e.state.clone(), e.observer.as_ref().map(|o| o.identity()))
            })
        };
        if let Some((mut state, identity)) = old {
            let action = state.process(cx.now(), tcp);
            if action == TimeWaitAction::Reopen && tcp.max_seg_size != Some(0) {
                let listener = self.iter().find_map(|(h, s)| {
                    let s = tcp::Socket::downcast(s)?;
                    (s.is_listening()
                        && s.accepts_ingress(meta)
                        && s.accepts(cx, ip, tcp)
                        && state.can_reopen_on(tcp, s))
                    .then_some(h)
                });
                if let Some(h) = listener {
                    self.get_mut::<tcp::Socket>(h).process_with_reuse(
                        cx,
                        meta,
                        ip,
                        tcp,
                        Some((&state, identity)),
                    );
                    if self.get::<tcp::Socket>(h).state() == State::SynReceived {
                        if let Some(h) = full {
                            self.get_mut::<tcp::Socket>(h).retire_time_wait();
                        }
                        if let Some(k) = key {
                            self.time_wait.remove(k).unwrap().retire();
                        }
                        return Some(None);
                    }
                }
            }
            if action == TimeWaitAction::Remove {
                if let Some(h) = full {
                    self.get_mut::<tcp::Socket>(h).retire_time_wait();
                }
                if let Some(k) = key {
                    self.time_wait.remove(k).unwrap().retire();
                }
                // A genuinely expired entry must not block a new SYN.
                if state.expires <= cx.now() {
                    return None;
                }
                return Some(None);
            }
            let reply = if matches!(action, TimeWaitAction::Ack | TimeWaitAction::Reopen) {
                state.reply(cx.now(), tcp)
            } else {
                None
            };
            let tx = state.meta();
            if let Some(h) = full {
                self.get_mut::<tcp::Socket>(h).time_wait = Some(state);
            } else if let Some(k) = key {
                self.time_wait.refresh(k, state);
            }
            return Some(reply.map(|(ip, tcp)| (tx, ip, tcp)));
        }
        None
    }
}
