use alloc::vec::Vec;
use core::{
    cmp::Ordering,
    sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
};

use smoltcp::wire::{EthernetAddress, IpAddress};
use system_error::SystemError;

use crate::{
    libs::rwsem::{RwSem, RwSemReadGuard},
    net::rtnl::RtnlGuard,
};

use super::{
    NeighborEntry, NeighborMutationOutcome, NeighborNewFlags, NeighborUpdate, NTF_ROUTER,
    NUD_NOARP, NUD_PERMANENT, RTN_UNICAST,
};

/// Authoritative configured-neighbor state for one network namespace.
///
/// Entries are sorted by `(ifindex, destination)`. Configured writes are rare,
/// while both dump and routed output need deterministic, allocation-free
/// lookup, making a compact sorted vector a better fit than a node-allocating
/// tree or a second per-interface projection.
#[derive(Debug)]
pub(crate) struct NeighborTable {
    entries: RwSem<Vec<NeighborEntry>>,
    /// Lock-free gate for the IPv4 egress path. The exact mapping remains in
    /// `entries`; this count only decides whether a poll must consult it.
    ipv4_entries: AtomicUsize,
}

/// Allocation-free view for packet paths that perform multiple lookups while
/// holding a non-sleeping queue lock. The sleepable read lock is acquired once
/// before that queue lock, making the lock order explicit.
pub(crate) struct NeighborReadGuard<'a> {
    entries: RwSemReadGuard<'a, Vec<NeighborEntry>>,
}

/// Immutable sorted snapshot shared by dump and proc readers.
pub(crate) struct NeighborSnapshot {
    entries: Vec<NeighborEntry>,
}

/// An RTNL-serialized, allocation-complete purge of one interface's configured
/// neighbors.
///
/// The copied entries are both the eventual notification payload and proof of
/// the table generation prepared for publication. Configured-neighbor writers
/// all require RTNL, so the matching table range cannot change between
/// `prepare_iface_purge` and `publish_iface_purge` while the caller retains the
/// same guard.
pub(super) struct PreparedNeighborPurge {
    ifindex: u32,
    removed: Vec<NeighborEntry>,
}

impl NeighborSnapshot {
    pub(crate) fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &NeighborEntry> {
        self.entries.iter()
    }

    pub(crate) fn contains(&self, ifindex: u32, destination: IpAddress) -> bool {
        find(&self.entries, ifindex, destination).is_ok()
    }

    pub(crate) fn for_iface(&self, ifindex: u32) -> &[NeighborEntry] {
        &self.entries[iface_range(&self.entries, ifindex)]
    }
}

impl NeighborReadGuard<'_> {
    pub(crate) fn lookup(&self, ifindex: u32, destination: IpAddress) -> Option<EthernetAddress> {
        let index = find(&self.entries, ifindex, destination).ok()?;
        let entry = self.entries[index];
        entry.ethernet_output.then_some(entry.lladdr).flatten()
    }
}

impl NeighborTable {
    pub(crate) fn new() -> Self {
        Self {
            entries: RwSem::new(Vec::new()),
            ipv4_entries: AtomicUsize::new(0),
        }
    }

    pub(super) fn add(
        &self,
        _rtnl: &RtnlGuard,
        update: NeighborUpdate,
        flags: NeighborNewFlags,
        ethernet_output: bool,
    ) -> Result<NeighborMutationOutcome, SystemError> {
        let mut entries = self.entries.write();
        match find(&entries, update.ifindex, update.destination) {
            Ok(index) => {
                let old = entries[index];
                if flags.excl {
                    return Err(SystemError::EEXIST);
                }
                validate_supported_capabilities(update)?;

                // Linux removes NEIGH_UPDATE_F_OVERRIDE and
                // NEIGH_UPDATE_F_OVERRIDE_ISROUTER without NLM_F_REPLACE. A
                // different valid lladdr therefore makes the entire request a
                // successful no-op, including its requested state.
                if update
                    .lladdr
                    .is_some_and(|lladdr| Some(lladdr) != old.lladdr && !flags.replace)
                {
                    return Ok(NeighborMutationOutcome::Unchanged(old));
                }

                let mut new = old;
                if let Some(lladdr) = update.lladdr {
                    new.lladdr = Some(lladdr);
                }
                new.state = update.state;
                if flags.replace {
                    new.flags = update.flags;
                }
                if new == old {
                    return Ok(NeighborMutationOutcome::Unchanged(old));
                }
                entries[index] = new;
                Ok(NeighborMutationOutcome::Updated { old, new })
            }
            Err(index) => {
                if !flags.create {
                    return Err(SystemError::ENOENT);
                }
                validate_supported_capabilities(update)?;
                let lladdr = if ethernet_output {
                    Some(update.lladdr.ok_or(SystemError::EINVAL)?)
                } else {
                    None
                };
                entries.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
                let entry = NeighborEntry {
                    ifindex: update.ifindex,
                    destination: update.destination,
                    lladdr,
                    ethernet_output,
                    state: update.state,
                    flags: update.flags,
                    kind: RTN_UNICAST,
                };
                if entry.ethernet_output && matches!(entry.destination, IpAddress::Ipv4(_)) {
                    // Publish the conservative slow-path gate before the
                    // non-fallible insert, so a packet cannot miss a committed
                    // configured mapping.
                    self.ipv4_entries.fetch_add(1, AtomicOrdering::Release);
                }
                entries.insert(index, entry);
                Ok(NeighborMutationOutcome::Added(entry))
            }
        }
    }

    pub(super) fn delete(
        &self,
        _rtnl: &RtnlGuard,
        ifindex: u32,
        destination: IpAddress,
    ) -> Result<NeighborEntry, SystemError> {
        let mut entries = self.entries.write();
        let index = find(&entries, ifindex, destination).map_err(|_| SystemError::ENOENT)?;
        let removed = entries.remove(index);
        if removed.ethernet_output && matches!(removed.destination, IpAddress::Ipv4(_)) {
            self.ipv4_entries.fetch_sub(1, AtomicOrdering::Release);
        }
        Ok(removed)
    }

    pub(super) fn lookup(&self, ifindex: u32, destination: IpAddress) -> Option<EthernetAddress> {
        self.read().lookup(ifindex, destination)
    }

    pub(super) fn contains(&self, ifindex: u32, destination: IpAddress) -> bool {
        find(&self.entries.read(), ifindex, destination).is_ok()
    }

    pub(super) fn read(&self) -> NeighborReadGuard<'_> {
        NeighborReadGuard {
            entries: self.entries.read(),
        }
    }

    pub(super) fn snapshot(&self) -> Result<NeighborSnapshot, SystemError> {
        let entries = self.entries.read();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(entries.len())
            .map_err(|_| SystemError::ENOMEM)?;
        snapshot.extend_from_slice(&entries);
        Ok(NeighborSnapshot { entries: snapshot })
    }

    pub(super) fn has_ipv4_entries(&self) -> bool {
        self.ipv4_entries.load(AtomicOrdering::Acquire) != 0
    }

    pub(super) fn prepare_iface_purge(
        &self,
        _rtnl: &RtnlGuard,
        ifindex: u32,
    ) -> Result<PreparedNeighborPurge, SystemError> {
        let entries = self.entries.read();
        let range = iface_range(&entries, ifindex);
        let mut removed = Vec::new();
        removed
            .try_reserve_exact(range.len())
            .map_err(|_| SystemError::ENOMEM)?;
        removed.extend_from_slice(&entries[range]);
        Ok(PreparedNeighborPurge { ifindex, removed })
    }

    pub(super) fn publish_iface_purge(
        &self,
        _rtnl: &RtnlGuard,
        prepared: PreparedNeighborPurge,
    ) -> Vec<NeighborEntry> {
        let PreparedNeighborPurge { ifindex, removed } = prepared;
        if removed.is_empty() {
            return removed;
        }

        let mut entries = self.entries.write();
        let range = iface_range(&entries, ifindex);
        debug_assert_eq!(&entries[range.clone()], removed.as_slice());
        self.remove_iface_entries(&mut entries, range);
        removed
    }

    /// Purges device-owned entries during an RTNL-serialized topology removal.
    /// Shrinking a vector is non-fallible, which lets device teardown finish
    /// after its fallible route preparation has succeeded.
    pub(super) fn remove_iface(&self, _rtnl: &RtnlGuard, ifindex: u32) {
        let mut entries = self.entries.write();
        let range = iface_range(&entries, ifindex);
        self.remove_iface_entries(&mut entries, range);
    }

    fn remove_iface_entries(
        &self,
        entries: &mut Vec<NeighborEntry>,
        range: core::ops::Range<usize>,
    ) {
        let removed_ipv4 = entries[range.clone()]
            .iter()
            .filter(|entry| {
                entry.ethernet_output && matches!(entry.destination, IpAddress::Ipv4(_))
            })
            .count();
        entries.drain(range);
        if removed_ipv4 != 0 {
            // Keep a stale-positive gate until the mappings are gone. A packet
            // may take the slow path unnecessarily, but can never miss a
            // configured mapping that is still published.
            self.ipv4_entries
                .fetch_sub(removed_ipv4, AtomicOrdering::Release);
        }
    }
}

fn validate_supported_capabilities(update: NeighborUpdate) -> Result<(), SystemError> {
    if !matches!(update.state, NUD_PERMANENT | NUD_NOARP)
        || update.flags & !NTF_ROUTER != 0
        || update.protocol != 0
        || update.flags_ext != 0
    {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    } else {
        Ok(())
    }
}

fn find(entries: &[NeighborEntry], ifindex: u32, destination: IpAddress) -> Result<usize, usize> {
    entries.binary_search_by(|entry| compare_key(entry, ifindex, destination))
}

fn iface_range(entries: &[NeighborEntry], ifindex: u32) -> core::ops::Range<usize> {
    let start = entries.partition_point(|entry| entry.ifindex < ifindex);
    let end = entries.partition_point(|entry| entry.ifindex <= ifindex);
    start..end
}

fn compare_key(entry: &NeighborEntry, ifindex: u32, destination: IpAddress) -> Ordering {
    entry
        .ifindex
        .cmp(&ifindex)
        .then_with(|| compare_ip(entry.destination, destination))
}

fn compare_ip(left: IpAddress, right: IpAddress) -> Ordering {
    match (left, right) {
        (IpAddress::Ipv4(left), IpAddress::Ipv4(right)) => left.octets().cmp(&right.octets()),
        (IpAddress::Ipv6(left), IpAddress::Ipv6(right)) => left.octets().cmp(&right.octets()),
        (IpAddress::Ipv4(_), IpAddress::Ipv6(_)) => Ordering::Less,
        (IpAddress::Ipv6(_), IpAddress::Ipv4(_)) => Ordering::Greater,
    }
}
