//! Authoritative configured-neighbor state.
//!
//! Dynamic ARP/ND learning remains owned by smoltcp. This module owns only
//! non-aging neighbors configured through the Linux control plane and exposes
//! the same state to dumps, procfs, and the routed IPv4 output path.

mod table;
mod types;

use alloc::{string::String, sync::Arc, vec::Vec};

use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress};
use system_error::SystemError;

use crate::{
    driver::net::{Iface, IfaceCommon},
    net::{
        routing::uapi::arp::{ArpFlags, ArpHrd},
        rtnl::RtnlGuard,
    },
    process::{namespace::net_namespace::NetNamespace, ProcessManager},
};

pub(crate) use table::{NeighborReadGuard, NeighborSnapshot, NeighborTable};
pub(crate) use types::{
    NeighborEntry, NeighborMutationOutcome, NeighborNewFlags, NeighborUpdate, NTF_ROUTER,
    NUD_FAILED, NUD_NOARP, NUD_PERMANENT, RTN_UNICAST,
};

const NTF_PROXY: u8 = 0x08;

/// ARP entry consumed by `/proc/net/arp`.
#[derive(Debug, Clone)]
pub struct ArpEntry {
    pub ip_addr: IpAddress,
    pub hw_type: ArpHrd,
    pub flags: ArpFlags,
    pub hw_addr: HardwareAddress,
    pub device: String,
}

pub(crate) fn add(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    mut update: NeighborUpdate,
    flags: NeighborNewFlags,
) -> Result<NeighborMutationOutcome, SystemError> {
    if !iface
        .net_namespace()
        .is_some_and(|owner| Arc::ptr_eq(&owner, netns))
    {
        return Err(SystemError::ENODEV);
    }
    validate_iface_and_update(iface, update)?;
    let ethernet_output = iface.common().type_() == crate::driver::net::types::InterfaceType::ETHER;
    if !ethernet_output {
        update.lladdr = None;
    }
    let outcome = netns
        .neighbor_table()
        .add(rtnl, update, flags, ethernet_output)?;
    let committed = match outcome {
        NeighborMutationOutcome::Added(entry) => Some(entry),
        NeighborMutationOutcome::Updated { new, .. } => Some(new),
        NeighborMutationOutcome::Unchanged(_) => None,
    };
    if let Some(entry) = committed {
        release_deferred_neighbor(iface, entry);
    }
    Ok(outcome)
}

pub(crate) fn delete(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    destination: IpAddress,
) -> Result<NeighborEntry, SystemError> {
    if !iface
        .net_namespace()
        .is_some_and(|owner| Arc::ptr_eq(&owner, netns))
    {
        return Err(SystemError::ENODEV);
    }
    let ifindex = iface.nic_id() as u32;
    if !netns.neighbor_table().contains(ifindex, destination) {
        return Err(SystemError::ENOENT);
    }

    // Keep the configured entry authoritative while retiring any stale
    // dynamic mapping. Packet paths therefore cannot observe a table miss and
    // revive that mapping between these two steps. RTNL serializes writers.
    let mut interface = iface.smol_iface().lock();
    interface.invalidate_neighbor(destination);
    let removed = netns.neighbor_table().delete(rtnl, ifindex, destination);
    drop(interface);
    removed
}

pub(crate) fn snapshot(netns: &Arc<NetNamespace>) -> Result<NeighborSnapshot, SystemError> {
    netns.neighbor_table().snapshot()
}

pub(crate) fn lookup(
    netns: &Arc<NetNamespace>,
    ifindex: u32,
    destination: IpAddress,
) -> Option<EthernetAddress> {
    netns.neighbor_table().lookup(ifindex, destination)
}

pub(crate) fn read(netns: &Arc<NetNamespace>) -> NeighborReadGuard<'_> {
    netns.neighbor_table().read()
}

pub(crate) fn has_ipv4_entries(netns: &Arc<NetNamespace>) -> bool {
    netns.neighbor_table().has_ipv4_entries()
}

/// Allocation-complete removal of all configured neighbors owned by one
/// interface. Holding the borrowed RTNL guard keeps the prepared table
/// generation stable until publication.
pub(crate) struct PreparedConfiguredNeighborPurge<'rtnl> {
    rtnl: &'rtnl RtnlGuard,
    netns: Arc<NetNamespace>,
    prepared: table::PreparedNeighborPurge,
}

impl PreparedConfiguredNeighborPurge<'_> {
    /// Publishes the purge without allocating and returns the removed entries
    /// for post-commit rtnetlink notifications.
    pub(crate) fn publish(self) -> Vec<NeighborEntry> {
        self.netns
            .neighbor_table()
            .publish_iface_purge(self.rtnl, self.prepared)
    }
}

pub(crate) fn prepare_configured_iface_purge<'rtnl>(
    rtnl: &'rtnl RtnlGuard,
    netns: &Arc<NetNamespace>,
    ifindex: u32,
) -> Result<PreparedConfiguredNeighborPurge<'rtnl>, SystemError> {
    let prepared = netns.neighbor_table().prepare_iface_purge(rtnl, ifindex)?;
    Ok(PreparedConfiguredNeighborPurge {
        rtnl,
        netns: netns.clone(),
        prepared,
    })
}

pub(crate) fn remove_iface(rtnl: &RtnlGuard, netns: &Arc<NetNamespace>, ifindex: u32) {
    netns.neighbor_table().remove_iface(rtnl, ifindex);
}

fn validate_iface_and_update(
    iface: &Arc<dyn Iface>,
    update: NeighborUpdate,
) -> Result<(), SystemError> {
    if iface.nic_id() as u32 != update.ifindex {
        return Err(SystemError::ENODEV);
    }
    // Proxy neighbors live in a distinct Linux table and must never fall
    // through to normal-key lifecycle handling when that table is absent.
    if update.flags & NTF_PROXY != 0 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    match update.destination {
        IpAddress::Ipv4(address)
            if address.is_unspecified()
                || address.is_multicast()
                || address == smoltcp::wire::Ipv4Address::BROADCAST =>
        {
            Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
        }
        IpAddress::Ipv6(address) if address.is_unspecified() || address.is_multicast() => {
            Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
        }
        IpAddress::Ipv4(_) | IpAddress::Ipv6(_) => Ok(()),
    }
}

fn release_deferred_neighbor(iface: &Arc<dyn Iface>, entry: NeighborEntry) {
    if !entry.ethernet_output {
        return;
    }
    let IpAddress::Ipv4(next_hop) = entry.destination else {
        return;
    };
    iface
        .common()
        .configured_neighbor_committed(entry.ifindex, next_hop);
}

/// Rechecks the authoritative table after a deferred bucket has become
/// visible, closing the completion-before-enqueue race with RTNL add.
pub(crate) fn release_deferred_after_enqueue(
    netns: &Arc<NetNamespace>,
    common: &IfaceCommon,
    ifindex: u32,
    next_hop: smoltcp::wire::Ipv4Address,
) -> bool {
    if lookup(netns, ifindex, IpAddress::Ipv4(next_hop)).is_none() {
        return false;
    }
    common.release_configured_neighbor(ifindex, next_hop)
}

/// Returns the current netns ARP view. Configured permanent entries shadow a
/// dynamic smoltcp entry with the same `(ifindex, IPv4)` key. Linux omits
/// NUD_NOARP entries from `/proc/net/arp`.
pub fn get_arp_entries() -> Vec<ArpEntry> {
    let netns = ProcessManager::current_netns();
    let configured = match snapshot(&netns) {
        Ok(entries) => entries,
        Err(error) => {
            log::warn!("failed to snapshot configured ARP entries: {:?}", error);
            // A dynamic-only fallback could expose a stale MAC that an
            // authoritative configured entry should shadow. Empty output is
            // the only consistency-preserving fallback for this infallible
            // procfs interface.
            return Vec::new();
        }
    };
    let devices = netns.device_list();
    let mut entries = Vec::new();

    for (_, iface) in devices.iter() {
        let ifindex = iface.nic_id() as u32;
        let dev_name = iface.iface_name();
        for neighbor in configured.for_iface(ifindex).iter().filter(|entry| {
            entry.state != NUD_NOARP && matches!(entry.destination, IpAddress::Ipv4(_))
        }) {
            entries.push(ArpEntry {
                ip_addr: neighbor.destination,
                hw_type: match iface.common().type_() {
                    crate::driver::net::types::InterfaceType::LOOPBACK => ArpHrd::Loopback,
                    _ => ArpHrd::Ethernet,
                },
                flags: ArpFlags::COM,
                hw_addr: HardwareAddress::Ethernet(neighbor.lladdr.unwrap_or_default()),
                device: dev_name.clone(),
            });
        }

        let smol_iface = iface.smol_iface().lock();
        let inner = &smol_iface.inner;
        let timestamp = inner.now();
        for (ip_addr, neighbor) in inner.neighbor_cache().iter() {
            if timestamp >= neighbor.expires_at {
                continue;
            }
            let IpAddress::Ipv4(ipv4) = *ip_addr else {
                continue;
            };
            if configured.contains(ifindex, IpAddress::Ipv4(ipv4)) {
                continue;
            }
            entries.push(ArpEntry {
                ip_addr: IpAddress::Ipv4(ipv4),
                hw_type: ArpHrd::Ethernet,
                flags: ArpFlags::COM,
                hw_addr: neighbor.hardware_addr,
                device: dev_name.clone(),
            });
        }
    }

    entries
}
