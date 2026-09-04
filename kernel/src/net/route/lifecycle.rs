use alloc::{sync::Arc, vec::Vec};

use smoltcp::{
    iface::Route as SmolRoute,
    wire::{IpAddress, IpCidr, Ipv4Cidr, Ipv6Cidr},
};
use system_error::SystemError;

use crate::{
    driver::net::{types::InterfaceFlags, AddressMetadata, Iface},
    net::address::CandidateAddressOwnership,
    net::rtnl::RtnlGuard,
    process::namespace::net_namespace::NetNamespace,
};

use super::{
    canonical_cidr, is_ipv4, prepare_with_devices, projection_for_iface, transact_with_devices,
    validate_entry_on_iface, FibEditor, FibTable, PreparedTransaction, ProjectionPlan, RouteEntry,
    RouteNotifications, RTN_BROADCAST, RTN_LOCAL, RTN_UNICAST, RTPROT_KERNEL, RT_SCOPE_HOST,
    RT_SCOPE_LINK, RT_SCOPE_UNIVERSE, RT_TABLE_LOCAL, RT_TABLE_MAIN,
};

pub(crate) fn register_iface(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    devices: &[Arc<dyn Iface>],
) -> Result<(), SystemError> {
    let addresses = try_clone_slice(&iface.common().ip_addrs())?;
    let staged = iface.common().take_bootstrap_routes();
    // Construction may preload addresses before the device is visible, but a
    // DOWN device must not publish those addresses into the FIB merely by
    // being registered. Explicit address changes after registration use the
    // address transaction path and remain independent of this rule.
    let defer_constructor_address_routes = !iface.flags().contains(InterfaceFlags::UP);
    iface
        .smol_iface()
        .lock()
        .set_route_table_includes_connected_prefixes(true);
    let result = transact_with_devices(rtnl, netns, devices, |candidate| {
        if !defer_constructor_address_routes {
            for route in derived_address_entries(iface, &addresses)? {
                candidate.insert_derived(route)?;
            }
        }
        for route in staged.iter().copied() {
            let route = RouteEntry {
                destination: canonical_cidr(route.destination),
                source: route.source.map(canonical_cidr),
                preferred_source: route.preferred_source,
                table: route.table,
                priority: route.priority,
                tos: route.tos,
                protocol: route.protocol,
                scope: route.scope,
                kind: route.kind,
                oif: route.oif,
                gateway: route.gateway,
                nexthop_flags: route.nexthop_flags,
            };
            validate_entry_on_iface(netns, iface, route)?;
            candidate.insert_derived(route)?;
        }
        Ok(())
    });
    if let Err(error) = result {
        iface.common().restore_bootstrap_routes(staged);
        iface
            .smol_iface()
            .lock()
            .set_route_table_includes_connected_prefixes(false);
        return Err(error);
    }
    Ok(())
}

pub(crate) fn unregister_iface(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    ifindex: u32,
    devices: &[Arc<dyn Iface>],
) -> Result<(), SystemError> {
    transact_with_devices(rtnl, netns, devices, |candidate| {
        candidate.remove_where(|route| route.oif == ifindex)?;
        Ok(())
    })
}

/// Applies Linux's IPv4 device-state FIB lifecycle. Ordinary NETDEV_DOWN
/// purges are intentionally silent at the rtnetlink notification boundary.
pub(crate) struct PreparedLinkStateChange<'rtnl> {
    transaction: PreparedTransaction<'rtnl, RouteNotifications>,
}

impl PreparedLinkStateChange<'_> {
    /// Publishes only preallocated state. RTNL guarantees that the FIB and
    /// topology still match the snapshot captured during preparation.
    pub(crate) fn publish(
        self,
        netns: &Arc<NetNamespace>,
        is_up: bool,
        publish_link_state: impl FnOnce(),
    ) -> RouteNotifications {
        if is_up {
            self.transaction
                .publish_around(netns, || {}, publish_link_state)
        } else {
            self.transaction
                .publish_around(netns, publish_link_state, || {})
        }
    }
}

pub(crate) fn prepare_link_state_change<'rtnl>(
    rtnl: &'rtnl RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    is_up: bool,
) -> Result<PreparedLinkStateChange<'rtnl>, SystemError> {
    let device_list = netns.device_list();
    let mut devices = Vec::new();
    devices
        .try_reserve_exact(device_list.len())
        .map_err(|_| SystemError::ENOMEM)?;
    devices.extend(device_list.values().cloned());
    drop(device_list);
    let ifindex = iface.nic_id() as u32;
    let transaction = prepare_with_devices(rtnl, netns, &devices, |candidate| {
        if is_up {
            let mut added = Vec::new();
            for entry in
                derived_address_entries_for_link_state(iface, &iface.common().ip_addrs(), true)?
            {
                // insert_derived() is the lifecycle authority here: the first
                // UP publishes every deferred constructor address route,
                // while later UP transitions only restore entries actually
                // removed on DOWN.
                if candidate.insert_derived(entry)? {
                    added.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
                    added.push(entry);
                }
            }
            Ok(RouteNotifications {
                removed: Vec::new(),
                added,
            })
        } else {
            let mut removed = candidate.remove_where(|entry| {
                entry.oif == ifindex
                    && !(is_ipv4(entry.destination.address())
                        && entry.table == RT_TABLE_LOCAL
                        && entry.kind == RTN_LOCAL
                        && entry.scope == RT_SCOPE_HOST)
            })?;
            // Linux emits IPv6 route deletion notifications from fib6_ifdown.
            // IPv4 link-down aliases are withdrawn silently by fib_flush().
            removed.retain(|entry| !is_ipv4(entry.destination.address()));
            Ok(RouteNotifications {
                removed,
                added: Vec::new(),
            })
        }
    })?;
    Ok(PreparedLinkStateChange { transaction })
}

/// A fully prepared address/FIB commit. Construction may fail; publication is
/// intentionally non-fallible and keeps addresses, projection and FIB atomic
/// under the established RTNL -> FIB -> smoltcp lock order.
struct PreparedAddressRouteCommit {
    before: FibTable,
    candidate: FibTable,
    projection: Vec<SmolRoute>,
    other_projections: ProjectionPlan,
    notifications: RouteNotifications,
    after_addresses: Vec<IpCidr>,
    metadata: Vec<AddressMetadata>,
}

impl PreparedAddressRouteCommit {
    fn prepare(
        netns: &Arc<NetNamespace>,
        iface: &Arc<dyn Iface>,
        before_addresses: &[IpCidr],
        after_addresses: &[IpCidr],
        metadata: Vec<AddressMetadata>,
        deleted_address: Option<IpAddress>,
    ) -> Result<Self, SystemError> {
        let ifindex = iface.nic_id() as u32;
        let before = netns.router().fib.read().try_clone()?;
        let mut candidate = before.try_clone()?;
        let before_routes = derived_address_entries(iface, before_addresses)?;
        let after_routes = derived_address_entries(iface, after_addresses)?;
        let device_list = netns.device_list();
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(device_list.len())
            .map_err(|_| SystemError::ENOMEM)?;
        devices.extend(device_list.values().cloned());
        let mut editor = FibEditor::new(&mut candidate);
        let mut silent = {
            let ownership = CandidateAddressOwnership::new(&device_list, iface, after_addresses);
            editor.reconcile_address_routes(
                &before_routes,
                &after_routes,
                ifindex,
                deleted_address,
                |entry| {
                    ownership.source_usable_on_oif(
                        entry.oif,
                        entry.preferred_source.expect(
                            "preferred-source filter only calls this predicate for a match",
                        ),
                    )
                },
            )
        }?;
        drop(device_list);
        let affected_oifs = editor.finish()?;

        let mut address_capacity = 0;
        iface
            .smol_iface()
            .lock()
            .update_ip_addrs(|addresses| address_capacity = addresses.capacity());
        if after_addresses.len() > address_capacity {
            return Err(SystemError::ENOSPC);
        }

        let projection = projection_for_iface(&candidate, ifindex)?;
        let mut other_oifs = Vec::new();
        other_oifs
            .try_reserve_exact(affected_oifs.len())
            .map_err(|_| SystemError::ENOMEM)?;
        other_oifs.extend(affected_oifs.iter().copied().filter(|oif| *oif != ifindex));
        let other_projections =
            ProjectionPlan::prepare(&before, &candidate, &other_oifs, &devices)?;
        let mut notifications = candidate.delta_from(&before)?.into_notifications();
        silent.removed.sort_unstable();
        silent.added.sort_unstable();
        notifications
            .removed
            .retain(|route| silent.removed.binary_search(route).is_err());
        notifications
            .added
            .retain(|route| silent.added.binary_search(route).is_err());
        Ok(Self {
            before,
            candidate,
            projection,
            other_projections,
            notifications,
            after_addresses: try_clone_slice(after_addresses)?,
            metadata,
        })
    }

    fn publish(self, netns: &Arc<NetNamespace>, iface: &Arc<dyn Iface>) -> RouteNotifications {
        let Self {
            before,
            candidate,
            mut projection,
            other_projections,
            notifications,
            after_addresses,
            metadata,
        } = self;
        let router = netns.router();
        let mut current = router.fib_write();
        debug_assert_eq!(*current, before);
        let mut smol_iface = iface.smol_iface().lock();
        smol_iface.update_ip_addrs(|addresses| {
            addresses.clear();
            for cidr in after_addresses.iter().copied() {
                addresses
                    .push(cidr)
                    .expect("address candidate was capacity-checked");
            }
        });
        smol_iface.routes_mut().update(|routes| {
            core::mem::swap(routes, &mut projection);
        });
        drop(smol_iface);
        other_projections.publish();
        *iface.common().address_metadata().lock() = metadata;
        let mut mirror = iface.router_common().ip_addrs.write();
        *mirror = after_addresses;
        *current = candidate;
        notifications
    }
}

pub(crate) fn commit_addresses(
    _rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    before: &[IpCidr],
    after: &[IpCidr],
    metadata: Vec<AddressMetadata>,
    deleted_address: Option<IpAddress>,
) -> Result<RouteNotifications, SystemError> {
    let prepared = PreparedAddressRouteCommit::prepare(
        netns,
        iface,
        before,
        after,
        metadata,
        deleted_address,
    )?;
    Ok(prepared.publish(netns, iface))
}

fn derived_address_entries(
    iface: &Arc<dyn Iface>,
    addresses: &[IpCidr],
) -> Result<Vec<RouteEntry>, SystemError> {
    derived_address_entries_for_link_state(
        iface,
        addresses,
        iface.flags().contains(InterfaceFlags::UP),
    )
}

fn derived_address_entries_for_link_state(
    iface: &Arc<dyn Iface>,
    addresses: &[IpCidr],
    is_up: bool,
) -> Result<Vec<RouteEntry>, SystemError> {
    let mut result = Vec::new();
    for cidr in addresses.iter().copied() {
        for entry in entries_for_address(iface, cidr, primary_for_prefix(addresses, cidr), is_up)? {
            if !result.contains(&entry) {
                result.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
                result.push(entry);
            }
        }
    }
    Ok(result)
}

#[inline]
fn ipv4_prefix_is_zeronet(cidr: smoltcp::wire::Ipv4Cidr) -> bool {
    cidr.network().address().is_unspecified()
}

fn entries_for_address(
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
    primary: Option<IpAddress>,
    is_up: bool,
) -> Result<Vec<RouteEntry>, SystemError> {
    let ipv4 = is_ipv4(cidr.address());
    let loopback = iface.flags().contains(InterfaceFlags::LOOPBACK);
    let mut result = Vec::new();
    result
        .try_reserve_exact(3)
        .map_err(|_| SystemError::ENOMEM)?;
    let connected = RouteEntry {
        destination: canonical_cidr(cidr),
        source: None,
        preferred_source: ipv4.then_some(primary).flatten(),
        table: if ipv4 && loopback {
            RT_TABLE_LOCAL
        } else {
            RT_TABLE_MAIN
        },
        priority: if ipv4 { 0 } else { 256 },
        tos: 0,
        protocol: RTPROT_KERNEL,
        scope: if ipv4 && loopback {
            RT_SCOPE_HOST
        } else if ipv4 {
            RT_SCOPE_LINK
        } else {
            RT_SCOPE_UNIVERSE
        },
        kind: if ipv4 && loopback {
            RTN_LOCAL
        } else {
            RTN_UNICAST
        },
        oif: iface.nic_id() as u32,
        gateway: None,
        nexthop_flags: 0,
    };
    let add_connected = match cidr {
        IpCidr::Ipv4(cidr) => {
            (loopback || cidr.prefix_len() < 32)
                && !ipv4_prefix_is_zeronet(cidr)
                && (loopback || is_up)
        }
        // Linux keeps the address object and its host-local route while the
        // device is down, but defers the IPv6 prefix route until NETDEV_UP.
        IpCidr::Ipv6(_) => is_up,
    };
    if add_connected {
        result.push(connected);
    }

    let local = RouteEntry {
        destination: host_cidr(cidr.address()),
        preferred_source: ipv4.then_some(primary).flatten(),
        table: RT_TABLE_LOCAL,
        priority: 0,
        protocol: RTPROT_KERNEL,
        scope: if ipv4 {
            RT_SCOPE_HOST
        } else {
            RT_SCOPE_UNIVERSE
        },
        kind: RTN_LOCAL,
        ..connected
    };
    // DragonOS addresses are immediately usable (the IPv6 address path does
    // not yet model DAD), matching Linux IFA_F_NODAD semantics. Keep the host
    // route while down; unlike a connected route, it represents local
    // delivery and does not select the device for external output.
    if !result.contains(&local) {
        result.push(local);
    }
    if let IpCidr::Ipv4(cidr) = cidr {
        if cidr.prefix_len() < 31 && !ipv4_prefix_is_zeronet(cidr) && is_up {
            if let Some(broadcast) = cidr.broadcast() {
                result.push(RouteEntry {
                    destination: IpCidr::Ipv4(Ipv4Cidr::new(broadcast, 32)),
                    preferred_source: primary,
                    table: RT_TABLE_LOCAL,
                    priority: 0,
                    protocol: RTPROT_KERNEL,
                    scope: RT_SCOPE_LINK,
                    kind: RTN_BROADCAST,
                    ..connected
                });
            }
        }
    }
    Ok(result)
}

fn try_clone_slice<T: Clone>(source: &[T]) -> Result<Vec<T>, SystemError> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(source.len())
        .map_err(|_| SystemError::ENOMEM)?;
    result.extend_from_slice(source);
    Ok(result)
}

fn host_cidr(address: IpAddress) -> IpCidr {
    match address {
        IpAddress::Ipv4(address) => IpCidr::Ipv4(Ipv4Cidr::new(address, 32)),
        IpAddress::Ipv6(address) => IpCidr::Ipv6(Ipv6Cidr::new(address, 128)),
    }
}

fn primary_for_prefix(addresses: &[IpCidr], cidr: IpCidr) -> Option<IpAddress> {
    let prefix = canonical_cidr(cidr);
    addresses
        .iter()
        .find(|candidate| canonical_cidr(**candidate) == prefix)
        .map(|candidate| candidate.address())
}
