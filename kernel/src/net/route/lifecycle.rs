use alloc::{string::String, sync::Arc, vec::Vec};

use smoltcp::{
    iface::Route as SmolRoute,
    wire::{IpAddress, IpCidr, Ipv4Cidr, Ipv6Cidr},
};
use system_error::SystemError;

use crate::{
    driver::net::{types::InterfaceFlags, AddressMetadata, AddressState, Iface},
    net::address::CandidateAddressOwnership,
    net::{
        rtnl::{self, RtnlGuard},
        socket::netlink::notify_automatic_ipv6_route,
    },
    process::namespace::net_namespace::NetNamespace,
};

use super::{
    canonical_cidr,
    fib::{FibEdit, PlannedFibMutation},
    is_ipv4, prepare_with_devices, prepare_with_devices_from, projection_for_iface,
    transact_single, transact_with_devices, validate_entry_on_iface, FibEditor, FibTable,
    PreparedTransaction, ProjectionPlan, RouteEntry, RouteNotifications, RTN_BROADCAST, RTN_LOCAL,
    RTN_MULTICAST, RTN_UNICAST, RTPROT_KERNEL, RT_SCOPE_HOST, RT_SCOPE_LINK, RT_SCOPE_UNIVERSE,
    RT_TABLE_LOCAL, RT_TABLE_MAIN,
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
            let metadata = iface.common().address_metadata().lock();
            for route in derived_address_entries(iface, &addresses, &metadata)? {
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
                ipv6_address_local: false,
                ipv6_metadata: None,
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

pub(crate) struct PreparedIfaceUnregister<'rtnl> {
    transaction: PreparedTransaction<'rtnl, ()>,
}

impl PreparedIfaceUnregister<'_> {
    pub(crate) fn publish(self, netns: &Arc<NetNamespace>, remove_devices: impl FnOnce()) {
        self.transaction
            .publish_around(netns, || {}, remove_devices);
    }
}

pub(crate) fn prepare_unregister_ifaces_from<'rtnl>(
    rtnl: &'rtnl RtnlGuard,
    netns: &Arc<NetNamespace>,
    ifindices: &[u32],
    devices: &[Arc<dyn Iface>],
    staged_before: Option<crate::net::link::StagedLinkFib<'_>>,
) -> Result<PreparedIfaceUnregister<'rtnl>, SystemError> {
    let transaction = prepare_with_devices_from(
        rtnl,
        netns,
        devices,
        staged_before.as_ref().map(|fib| fib.candidate()),
        |candidate| {
            candidate.remove_where(|route| ifindices.contains(&route.oif))?;
            Ok(())
        },
    )?;
    Ok(PreparedIfaceUnregister { transaction })
}

/// Final netns destruction has no userspace request to roll back. Its live
/// peer must be removed even when an ordinary FIB snapshot cannot allocate.
pub(crate) fn purge_iface_for_netns_teardown(netns: &Arc<NetNamespace>, iface: &Arc<dyn Iface>) {
    let router = netns.router();
    let mut fib = router.fib_write();
    fib.purge_oif_for_netns_teardown(iface.nic_id() as u32);
    iface
        .smol_iface()
        .lock()
        .routes_mut()
        .update(|routes| routes.clear());
}

/// Applies Linux's IPv4 device-state FIB lifecycle. Ordinary NETDEV_DOWN
/// purges are intentionally silent at the rtnetlink notification boundary.
pub(crate) struct PreparedLinkStateChange<'rtnl> {
    transaction: PreparedTransaction<'rtnl, RouteNotifications>,
    ifindex: u32,
}

impl PreparedLinkStateChange<'_> {
    pub(in crate::net) fn candidate(&self) -> &FibTable {
        self.transaction.candidate()
    }

    /// Publishes only preallocated state. RTNL guarantees that the FIB and
    /// topology still match the snapshot captured during preparation.
    pub(crate) fn publish(
        self,
        netns: &Arc<NetNamespace>,
        is_up: bool,
        publish_link_state: impl FnOnce(),
    ) -> RouteNotifications {
        let notifications = if is_up {
            self.transaction
                .publish_around(netns, || {}, publish_link_state)
        } else {
            self.transaction
                .publish_around(netns, publish_link_state, || {})
        };
        if !is_up {
            netns.conntrack().invalidate_masquerade_oif(self.ifindex);
        }
        notifications
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
            let metadata = iface.common().address_metadata().lock();
            for entry in derived_address_entries_for_link_state(
                iface,
                &iface.common().ip_addrs(),
                &metadata,
                true,
            )? {
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
    Ok(PreparedLinkStateChange {
        transaction,
        ifindex,
    })
}

/// A fully prepared address/FIB commit. Construction may fail; publication is
/// intentionally non-fallible and keeps addresses, projection and FIB atomic
/// under the established RTNL -> FIB -> smoltcp lock order.
pub(crate) struct PreparedAddressRouteCommit {
    before: FibTable,
    candidate: FibTable,
    projection: Vec<SmolRoute>,
    other_projections: ProjectionPlan,
    notifications: RouteNotifications,
    after_addresses: Vec<IpCidr>,
    metadata: Vec<AddressMetadata>,
    deleted_addresses: Vec<IpAddress>,
    invalidate_masquerade_oif: bool,
    ipv6_enabled: Option<bool>,
}

#[derive(Clone, Copy)]
pub(crate) enum Ipv6PrefixChange {
    Upsert {
        address: Ipv6Cidr,
        expires_at_us: Option<i64>,
        replace_permanent: bool,
    },
    RemovePermanent(Ipv6Cidr),
}

impl Ipv6PrefixChange {
    fn address(self) -> Ipv6Cidr {
        match self {
            Self::Upsert { address, .. } | Self::RemovePermanent(address) => address,
        }
    }
}

pub(crate) struct AddressLinkChange<'a> {
    pub before: &'a [IpCidr],
    pub after: &'a [IpCidr],
    pub metadata: Vec<AddressMetadata>,
    pub deleted_addresses: &'a [IpAddress],
    pub ipv6_prefix_change: Option<Ipv6PrefixChange>,
    pub ensure_mroute: bool,
    pub was_up: bool,
    pub is_up: bool,
    pub ipv6_enabled: Option<bool>,
    pub withdraw_ipv6_routes: bool,
}

impl PreparedAddressRouteCommit {
    pub(in crate::net) fn candidate(&self) -> &FibTable {
        &self.candidate
    }

    fn prepare_for_link(
        netns: &Arc<NetNamespace>,
        iface: &Arc<dyn Iface>,
        change: AddressLinkChange<'_>,
        staged_before: Option<&FibTable>,
    ) -> Result<Self, SystemError> {
        let AddressLinkChange {
            before: before_addresses,
            after: after_addresses,
            metadata,
            deleted_addresses,
            ipv6_prefix_change,
            ensure_mroute,
            was_up,
            is_up,
            ipv6_enabled,
            withdraw_ipv6_routes,
        } = change;
        let was_ipv6_enabled =
            iface.common().ipv6_device_present() && iface.common().ipv6_enabled();
        let is_ipv6_enabled = ipv6_enabled.unwrap_or(was_ipv6_enabled);
        let ifindex = iface.nic_id() as u32;
        let link_down = was_up && !is_up;
        let disable_ipv6 = was_ipv6_enabled && !is_ipv6_enabled;
        let remove_on_down = |entry: RouteEntry| {
            entry.oif == ifindex
                && !(is_ipv4(entry.destination.address())
                    && entry.table == RT_TABLE_LOCAL
                    && entry.kind == RTN_LOCAL
                    && entry.scope == RT_SCOPE_HOST)
        };
        let withdraw_ipv6_routes = disable_ipv6 || withdraw_ipv6_routes;
        let remove_for_ipv6 = |entry: RouteEntry| {
            withdraw_ipv6_routes && entry.oif == ifindex && !is_ipv4(entry.destination.address())
        };
        let address_capacity = iface.smol_iface().lock().ip_addr_capacity();
        let inactive_count = metadata
            .iter()
            .filter(|entry| !entry.state.is_usable())
            .count();
        if after_addresses.len() + inactive_count > address_capacity {
            return Err(SystemError::ENOSPC);
        }

        let before = match staged_before {
            Some(staged) => staged.try_clone()?,
            None => netns.router().fib.read().try_clone()?,
        };
        let mut candidate = before.try_clone()?;
        let mut before_routes = {
            let old_metadata = iface.common().address_metadata().lock();
            derived_address_entries_for_link_state(iface, before_addresses, &old_metadata, was_up)?
        };
        let mut after_address_routes =
            derived_address_entries_for_link_state(iface, after_addresses, &metadata, was_up)?;
        let prefix_change = ipv6_prefix_change
            .filter(|_| is_up && !iface.flags().contains(InterfaceFlags::LOOPBACK))
            .map(|change| {
                let route = connected_address_route(iface, IpCidr::Ipv6(change.address()), None);
                let projected_elsewhere = |entry: &RouteEntry| {
                    !entry.is_address_prefix() || entry.destination != route.destination
                };
                before_routes.retain(projected_elsewhere);
                after_address_routes.retain(projected_elsewhere);
                (change, route)
            });
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
                &after_address_routes,
                ifindex,
                deleted_addresses,
                |entry| {
                    // Only surviving IPv6 routes need a silent prefsrc edit.
                    // Keep the separate IPv4 address-removal phase intact.
                    if !is_ipv4(entry.destination.address())
                        && ((link_down && remove_on_down(entry)) || remove_for_ipv6(entry))
                    {
                        return true;
                    }
                    let source = entry
                        .preferred_source
                        .expect("preferred-source filter only calls this predicate for a match");
                    ownership.source_usable_on_oif(
                        entry.oif,
                        source,
                        super::validation::preferred_source_requires_iface(entry, source),
                    )
                },
            )
        }?;
        let prefix_replacement = match prefix_change {
            Some((
                Ipv6PrefixChange::Upsert {
                    expires_at_us,
                    replace_permanent,
                    ..
                },
                route,
            )) => editor.update_address_prefix(
                route.with_address_prefix_deadline(expires_at_us),
                replace_permanent,
            )?,
            Some((Ipv6PrefixChange::RemovePermanent(_), route)) => {
                let siblings = metadata
                    .iter()
                    .filter(|entry| canonical_cidr(entry.cidr) == route.destination);
                if siblings.clone().any(|entry| entry.ipv6_permanent) {
                    None
                } else {
                    let deadline = siblings
                        .filter_map(|entry| entry.lifetimes?.valid_until_us())
                        .max();
                    editor.remove_permanent_address_prefix(route, deadline)?
                }
            }
            None => None,
        };
        if ensure_mroute {
            if let Some(route) = ipv6_multicast_route(iface.as_ref(), is_up, is_ipv6_enabled) {
                editor.insert_derived(route)?;
            }
        }
        if withdraw_ipv6_routes {
            editor.remove_where(remove_for_ipv6)?;
        }
        let mut affected_oifs = editor.finish()?;
        let address_removed = if link_down {
            candidate.delta_from(&before)?.removed
        } else {
            Vec::new()
        };
        if link_down {
            let mut state_editor = FibEditor::new(&mut candidate);
            state_editor.remove_where(remove_on_down)?;
            for oif in state_editor.finish()? {
                if !affected_oifs.contains(&oif) {
                    affected_oifs
                        .try_reserve(1)
                        .map_err(|_| SystemError::ENOMEM)?;
                    affected_oifs.push(oif);
                }
            }
        } else if !was_up && is_up {
            let mut state_editor = FibEditor::new(&mut candidate);
            for entry in
                derived_address_entries_for_link_state(iface, after_addresses, &metadata, true)?
            {
                state_editor.insert_derived(entry)?;
            }
            for oif in state_editor.finish()? {
                if !affected_oifs.contains(&oif) {
                    affected_oifs
                        .try_reserve(1)
                        .map_err(|_| SystemError::ENOMEM)?;
                    affected_oifs.push(oif);
                }
            }
        }
        drop(device_list);

        let projection = projection_for_iface(&candidate, ifindex)?;
        let mut other_oifs = Vec::new();
        other_oifs
            .try_reserve_exact(affected_oifs.len())
            .map_err(|_| SystemError::ENOMEM)?;
        other_oifs.extend(affected_oifs.iter().copied().filter(|oif| *oif != ifindex));
        // A staged follow-up starts from a prepared FIB candidate, but the
        // live smoltcp projections remain at the original generation until
        // both plans are committed. Reserve against that live projection.
        let projection_before = if staged_before.is_some() {
            Some(netns.router().fib.read().try_clone()?)
        } else {
            None
        };
        let other_projections = ProjectionPlan::prepare(
            projection_before.as_ref().unwrap_or(&before),
            &candidate,
            &other_oifs,
            &devices,
        )?;
        let mut notifications = candidate.delta_from(&before)?.into_notifications();
        if link_down {
            // NETDEV_DOWN silently flushes ordinary IPv4 routes. Routes
            // removed by the preceding MTU address teardown retain their
            // normal address-lifecycle notifications.
            notifications.removed.retain(|entry| {
                !is_ipv4(entry.destination.address()) || address_removed.contains(entry)
            });
        }
        if let Some((old, new)) = prefix_replacement {
            notifications.removed.retain(|entry| *entry != old);
            notifications.added.retain(|entry| *entry != new);
        }
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
            deleted_addresses: try_clone_slice(deleted_addresses)?,
            invalidate_masquerade_oif: link_down,
            ipv6_enabled,
        })
    }

    pub(in crate::net) fn publish(
        self,
        netns: &Arc<NetNamespace>,
        iface: &Arc<dyn Iface>,
    ) -> RouteNotifications {
        self.publish_link_change(netns, iface, None, || {}, || {}, false)
    }

    pub(crate) fn publish_link_change(
        self,
        netns: &Arc<NetNamespace>,
        iface: &Arc<dyn Iface>,
        renamed_to: Option<String>,
        publish_mtu: impl FnOnce(),
        publish_link_state: impl FnOnce(),
        is_up: bool,
    ) -> RouteNotifications {
        let Self {
            before,
            candidate,
            mut projection,
            other_projections,
            notifications,
            after_addresses,
            metadata,
            deleted_addresses,
            invalidate_masquerade_oif,
            ipv6_enabled,
        } = self;
        let router = netns.router();
        let mut current = router.fib_write();
        let before_broadcasts = iface.common().configured_ipv4_broadcast_count();
        let after_broadcasts = crate::net::address::configured_ipv4_broadcast_count(&metadata);
        publish_mtu();
        let mut publish_link_state = Some(publish_link_state);
        if !is_up {
            publish_link_state
                .take()
                .expect("link state publisher runs once")();
        }
        debug_assert_eq!(*current, before);
        let mut smol_iface = iface.smol_iface().lock();
        let receive_filter_changed = if let Some(enabled) = ipv6_enabled {
            smol_iface.set_ipv6_enabled(enabled, &after_addresses);
            true
        } else if smol_iface.ip_addrs() != after_addresses.as_slice() {
            smol_iface.update_ip_addrs(|addresses| {
                addresses.clear();
                addresses
                    .extend_from_slice(&after_addresses)
                    .expect("address candidate was capacity-checked");
            });
            true
        } else {
            false
        };
        if receive_filter_changed {
            iface.common().sync_receive_filter(&smol_iface);
        }
        smol_iface.routes_mut().update(|routes| {
            core::mem::swap(routes, &mut projection);
        });
        crate::net::address::publish_ipv6_preference(&mut smol_iface, &metadata);
        drop(smol_iface);
        other_projections.publish();
        if let Some(name) = renamed_to {
            iface
                .common()
                .publish_name_and_address_metadata(name, metadata);
        } else {
            *iface.common().address_metadata().lock() = metadata;
        }
        let mut mirror = iface.router_common().ip_addrs.write();
        let had_link_local = mirror
            .iter()
            .any(|cidr| super::is_ipv6_link_local(cidr.address()));
        let has_link_local = after_addresses
            .iter()
            .any(|cidr| super::is_ipv6_link_local(cidr.address()));
        *mirror = after_addresses;
        *current = candidate;
        netns.publish_ipv4_broadcast_count_change(before_broadcasts, after_broadcasts);
        if is_up {
            publish_link_state
                .take()
                .expect("link state publisher runs once")();
        }
        drop(mirror);
        drop(current);
        if had_link_local != has_link_local {
            crate::net::address::ipv6_router_solicitation::address_changed(
                iface.as_ref(),
                has_link_local,
            );
        }
        let ifindex = iface.nic_id() as u32;
        if invalidate_masquerade_oif {
            netns.conntrack().invalidate_masquerade_oif(ifindex);
        } else {
            for address in deleted_addresses {
                let address = match address {
                    IpAddress::Ipv4(address) => address.octets().into(),
                    IpAddress::Ipv6(address) => address.octets().into(),
                };
                netns
                    .conntrack()
                    .invalidate_masquerade_address(ifindex, address);
            }
        }
        notifications
    }
}

pub(crate) fn prepare_address_link_change(
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    change: AddressLinkChange<'_>,
) -> Result<PreparedAddressRouteCommit, SystemError> {
    PreparedAddressRouteCommit::prepare_for_link(netns, iface, change, None)
}

pub(in crate::net) fn prepare_address_link_change_from(
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    change: AddressLinkChange<'_>,
    staged_before: &FibTable,
) -> Result<PreparedAddressRouteCommit, SystemError> {
    PreparedAddressRouteCommit::prepare_for_link(netns, iface, change, Some(staged_before))
}

fn derived_address_entries(
    iface: &Arc<dyn Iface>,
    addresses: &[IpCidr],
    metadata: &[AddressMetadata],
) -> Result<Vec<RouteEntry>, SystemError> {
    derived_address_entries_for_link_state(
        iface,
        addresses,
        metadata,
        iface.flags().contains(InterfaceFlags::UP),
    )
}

fn ipv6_multicast_route(iface: &dyn Iface, is_up: bool, ipv6_enabled: bool) -> Option<RouteEntry> {
    if !is_up
        || !ipv6_enabled
        || iface.mtu() < crate::net::address::IPV6_MIN_MTU
        || iface.flags().contains(InterfaceFlags::LOOPBACK)
    {
        return None;
    }
    Some(RouteEntry {
        destination: IpCidr::Ipv6(Ipv6Cidr::new(
            smoltcp::wire::Ipv6Address::from([0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            8,
        )),
        source: None,
        preferred_source: None,
        table: RT_TABLE_LOCAL,
        priority: 256,
        tos: 0,
        protocol: RTPROT_KERNEL,
        scope: RT_SCOPE_UNIVERSE,
        kind: RTN_MULTICAST,
        oif: iface.nic_id() as u32,
        gateway: None,
        nexthop_flags: 0,
        ipv6_address_local: false,
        ipv6_metadata: None,
    })
}

pub(crate) fn ensure_ipv6_multicast_route(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &dyn Iface,
) -> Result<Option<RouteEntry>, SystemError> {
    let Some(route) = ipv6_multicast_route(
        iface,
        iface.flags().contains(InterfaceFlags::UP),
        iface.common().ipv6_device_present() && iface.common().ipv6_enabled(),
    ) else {
        return Ok(None);
    };
    let edit =
        commit_automatic_ipv6_route_edit(rtnl, netns, |fib| fib.plan_multicast_route(route))?;
    Ok(matches!(edit, FibEdit::Insert { .. }).then_some(route))
}

fn derived_address_entries_for_link_state(
    iface: &Arc<dyn Iface>,
    addresses: &[IpCidr],
    metadata: &[AddressMetadata],
    is_up: bool,
) -> Result<Vec<RouteEntry>, SystemError> {
    let mut result = Vec::new();
    let addresses_with_metadata = addresses
        .iter()
        .copied()
        .map(|cidr| (cidr, metadata.iter().find(|entry| entry.cidr == cidr)))
        .chain(
            metadata
                .iter()
                .filter(|entry| !entry.state.is_usable())
                .map(|entry| (entry.cidr, Some(entry))),
        );
    for (cidr, metadata) in addresses_with_metadata {
        let state = metadata.map(|entry| entry.state).unwrap_or_default();
        let broadcast = metadata.and_then(|entry| entry.broadcast);
        for entry in entries_for_address(
            iface,
            cidr,
            primary_for_prefix(addresses, cidr),
            broadcast,
            is_up,
            state,
            metadata.is_none_or(|entry| entry.ipv6_permanent),
        )? {
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

fn connected_address_route(
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
    primary: Option<IpAddress>,
) -> RouteEntry {
    let ipv4 = is_ipv4(cidr.address());
    let loopback = iface.flags().contains(InterfaceFlags::LOOPBACK);
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
        ipv6_address_local: false,
        ipv6_metadata: None,
    };
    if ipv4 || loopback {
        connected
    } else {
        connected.with_address_prefix_deadline(None)
    }
}

fn entries_for_address(
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
    primary: Option<IpAddress>,
    broadcast: Option<smoltcp::wire::Ipv4Address>,
    is_up: bool,
    state: AddressState,
    ipv6_permanent: bool,
) -> Result<Vec<RouteEntry>, SystemError> {
    let ipv4 = is_ipv4(cidr.address());
    let loopback = iface.flags().contains(InterfaceFlags::LOOPBACK);
    let mut result = Vec::new();
    result
        .try_reserve_exact(4)
        .map_err(|_| SystemError::ENOMEM)?;
    let connected = connected_address_route(iface, cidr, primary);
    let add_connected = match cidr {
        IpCidr::Ipv4(cidr) => {
            (loopback || cidr.prefix_len() < 32)
                && !ipv4_prefix_is_zeronet(cidr)
                && (loopback || is_up)
        }
        // Finite prefixes on non-loopback links use independent lifetimes.
        // Retained loopback addresses have no active prefix until link-up.
        IpCidr::Ipv6(_) => {
            is_up
                && (loopback || ipv6_permanent)
                && !matches!(state, AddressState::Slaac(_))
                && !(loopback && matches!(state, AddressState::Retained(_)))
        }
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
        ipv6_address_local: !ipv4,
        ipv6_metadata: None,
        ..connected
    };
    // Keep the host route of a usable address while down. Unlike a
    // connected route, it represents local delivery rather than egress.
    if state.is_usable() && !result.contains(&local) {
        result.push(local);
    }
    if let IpCidr::Ipv4(cidr) = cidr {
        if is_up {
            if let Some(broadcast) =
                broadcast.filter(|address| !address.is_unspecified() && !address.is_broadcast())
            {
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
        if cidr.prefix_len() < 31 && !ipv4_prefix_is_zeronet(cidr) && is_up {
            if let Some(broadcast) = cidr.broadcast() {
                let route = RouteEntry {
                    destination: IpCidr::Ipv4(Ipv4Cidr::new(broadcast, 32)),
                    preferred_source: primary,
                    table: RT_TABLE_LOCAL,
                    priority: 0,
                    protocol: RTPROT_KERNEL,
                    scope: RT_SCOPE_LINK,
                    kind: RTN_BROADCAST,
                    ..connected
                };
                if !result.contains(&route) {
                    result.push(route);
                }
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

pub(super) fn apply_automatic_ipv6_route_edit(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    plan: impl FnOnce(&FibTable) -> FibEdit,
) -> Result<FibEdit, SystemError> {
    let edit = commit_automatic_ipv6_route_edit(rtnl, netns, plan)?;
    match edit {
        FibEdit::Insert { route, .. } => notify_automatic_ipv6_route(netns, route, false, false),
        FibEdit::Delete { route, .. } => notify_automatic_ipv6_route(netns, route, true, false),
        FibEdit::Replace { old, new, .. } if old.preference() != new.preference() => {
            notify_automatic_ipv6_route(netns, new, false, true);
        }
        _ => {}
    }
    Ok(edit)
}

fn commit_automatic_ipv6_route_edit(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    plan: impl FnOnce(&FibTable) -> FibEdit,
) -> Result<FibEdit, SystemError> {
    // RTNL keeps the edit stable until projection and FIB publication.
    let edit = plan(&netns.router().fib.read());
    if matches!(edit, FibEdit::None) {
        return Ok(edit);
    }
    transact_single(rtnl, netns, |_| {
        Ok(PlannedFibMutation { edit, outcome: () })
    })?;
    Ok(edit)
}

pub(crate) fn next_ipv6_route_deadline_us(netns: &NetNamespace) -> Option<i64> {
    netns
        .router()
        .fib
        .read()
        .expiring_ipv6_routes()
        .filter_map(RouteEntry::expires_at_us)
        .min()
}

/// Remove in bounded batches, retaining an earlier retry if projection
/// preparation fails. The FIB and smoltcp already ignore overdue routes.
pub(crate) fn expire_ipv6_routes(netns: &Arc<NetNamespace>, now_us: i64) -> Option<i64> {
    let rtnl = rtnl::lock();
    for _ in 0..16 {
        let route = netns
            .router()
            .fib
            .read()
            .expiring_ipv6_routes()
            .find(|route| !route.is_live_at(now_us));
        let Some(route) = route else {
            break;
        };
        if apply_automatic_ipv6_route_edit(&rtnl, netns, |fib| fib.plan_expired_route(route))
            .is_err()
        {
            return Some(now_us.saturating_add(10_000));
        }
    }
    next_ipv6_route_deadline_us(netns)
}
