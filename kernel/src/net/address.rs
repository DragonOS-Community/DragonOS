//! Protocol-independent interface address mutation core.
//!
//! Runtime callers must hold RTNL for the whole operation.  The smoltcp
//! address list, its connected-route projection, and the router compatibility
//! projection are committed under one smoltcp interface lock.

use alloc::{ffi::CString, string::String, sync::Arc, vec::Vec};

use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address, Ipv6Address, Ipv6AddressExt, Ipv6Cidr};
use system_error::SystemError;

use crate::{
    driver::net::{types::InterfaceFlags, AddressMetadata, AddressState, DadState, Iface},
    net::{route::Ipv6PrefixChange, rtnl::RtnlGuard},
    process::namespace::net_namespace::NetNamespace,
};

pub(crate) mod ipv6_autoconf;
pub(crate) mod ipv6_lifetime;
pub(in crate::net) mod ipv6_router_solicitation;
pub(crate) mod ipv6_slaac;

const IPV4_MIN_MTU: usize = 68;
pub(crate) const IPV6_MIN_MTU: usize = 1280;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AddressMutation {
    Add(IpCidr),
    Delete(IpCidr),
    Replace(IpCidr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AddressMutationOutcome {
    Added(IpCidr),
    Deleted(IpCidr),
    Replaced(IpCidr),
}

pub(in crate::net) struct AddressMutationCommit {
    pub outcome: AddressMutationOutcome,
    pub route_changes: crate::net::route::RouteNotifications,
}

#[derive(Default)]
pub(in crate::net) struct AddressOptions {
    pub label: Option<CString>,
    pub broadcast: Option<Ipv4Address>,
    pub ipv6_nodad: Option<bool>,
    pub ipv6_lifetimes: Option<ipv6_lifetime::ConfiguredLifetimes>,
}

pub(crate) struct AddressInfo {
    pub cidr: IpCidr,
    pub label: CString,
    pub broadcast: Option<Ipv4Address>,
    pub state: AddressState,
    pub ipv6_nodad: bool,
    pub ipv6_permanent: bool,
    pub lifetimes: Option<ipv6_lifetime::Lifetimes>,
}

pub(in crate::net) struct PreparedLinkAddressChange {
    routes: crate::net::route::PreparedAddressRouteCommit,
    pub(in crate::net) withdraws_ipv6: bool,
    removed: Vec<AddressInfo>,
    changed_addresses: Vec<IpCidr>,
}

fn prepare_address_removal(
    iface: &Arc<dyn Iface>,
    after: &mut Vec<IpCidr>,
    metadata: &mut Vec<AddressMetadata>,
    matches: impl Fn(IpCidr) -> bool,
    keep_retained: bool,
) -> Result<(Vec<AddressInfo>, Vec<IpAddress>), SystemError> {
    let metadata_matches = |entry: &AddressMetadata| {
        !entry.state.is_usable()
            && (!keep_retained || !matches!(entry.state, AddressState::Retained(_)))
            && matches(entry.cidr)
    };
    let removal_count = after.iter().filter(|&&cidr| matches(cidr)).count()
        + metadata
            .iter()
            .filter(|entry| metadata_matches(entry))
            .count();
    if removal_count == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    let default_label = try_default_label(iface)?;
    let mut removed = Vec::new();
    removed
        .try_reserve_exact(removal_count)
        .map_err(|_| SystemError::ENOMEM)?;
    let mut deleted_addresses = Vec::new();
    deleted_addresses
        .try_reserve_exact(removal_count)
        .map_err(|_| SystemError::ENOMEM)?;

    let mut index = 0;
    while index < after.len() {
        let cidr = after[index];
        if !matches(cidr) {
            index += 1;
            continue;
        }
        after.remove(index);
        deleted_addresses.push(cidr.address());
        let entry = metadata
            .iter()
            .position(|entry| entry.cidr == cidr)
            .map(|index| metadata.remove(index));
        removed.push(removed_address_info(cidr, entry, &default_label)?);
    }

    let mut index = 0;
    while index < metadata.len() {
        let entry = &metadata[index];
        if metadata_matches(entry) {
            let entry = metadata.remove(index);
            if matches!(entry.state, AddressState::Retained(_)) {
                deleted_addresses.push(entry.cidr.address());
            }
            removed.push(removed_address_info(
                entry.cidr,
                Some(entry),
                &default_label,
            )?);
        } else {
            index += 1;
        }
    }

    Ok((removed, deleted_addresses))
}

fn removed_address_info(
    cidr: IpCidr,
    entry: Option<AddressMetadata>,
    default_label: &CString,
) -> Result<AddressInfo, SystemError> {
    let entry = entry.unwrap_or_else(|| AddressMetadata::configured(cidr));
    Ok(AddressInfo {
        cidr,
        label: match entry.label {
            Some(label) => label,
            None => try_clone_cstring(default_label)?,
        },
        broadcast: entry.broadcast,
        state: entry.state,
        ipv6_nodad: entry.ipv6_nodad,
        ipv6_permanent: entry.ipv6_permanent,
        lifetimes: entry.lifetimes,
    })
}

pub(crate) fn set_device_ipv6_disabled(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    value: i32,
) -> Result<(), SystemError> {
    let enabled = value == 0;
    if iface.common().ipv6_enabled() == enabled {
        iface.common().publish_ipv6_disabled(value);
        return Ok(());
    }

    let flags = iface.common().flags();
    let up = flags.contains(InterfaceFlags::UP);
    let mirror = iface.router_common().ip_addrs.read();
    let restored = (enabled && up)
        .then(|| missing_loopback_ipv6_address(iface, iface.mtu(), &mirror))
        .flatten();
    let before = try_clone_copy_slice(&mirror, 0)?;
    let mut after = try_clone_copy_slice(&mirror, usize::from(restored.is_some()))?;
    drop(mirror);

    let mut metadata = try_clone_metadata(
        &iface.common().address_metadata().lock(),
        usize::from(restored.is_some()),
    )?;
    let (removed_addresses, deleted_addresses) = prepare_address_removal(
        iface,
        &mut after,
        &mut metadata,
        |cidr| !enabled && matches!(cidr, IpCidr::Ipv6(_)),
        false,
    )?;
    after.extend(restored);
    metadata.extend(restored.map(AddressMetadata::configured));
    let routes = crate::net::route::prepare_address_link_change(
        netns,
        iface,
        crate::net::route::AddressLinkChange {
            before: &before,
            after: &after,
            metadata,
            deleted_addresses: &deleted_addresses,
            ipv6_prefix_change: None,
            was_up: up,
            is_up: up,
            ipv6_enabled: Some(enabled),
            withdraw_ipv6_routes: false,
        },
    )?;
    let neighbors = if enabled {
        None
    } else {
        Some(crate::net::neighbor::prepare_configured_iface_purge(
            rtnl,
            netns,
            iface.nic_id() as u32,
            true,
        )?)
    };

    // Block IPv6 ingress before withdrawing its state. When enabling,
    // publish the state before opening ingress.
    let route_changes = routes.publish_link_change(
        netns,
        iface,
        None,
        || {},
        || iface.common().publish_ipv6_disabled(value),
        enabled,
    );
    if !enabled {
        ipv6_autoconf::cancel(iface.common());
        iface.common().fail_deferred_ipv6();
    } else {
        ipv6_autoconf::link_changed(iface.common(), iface.operstate());
    }
    let removed_neighbors = neighbors.map_or_else(Vec::new, |prepared| prepared.publish());

    crate::net::socket::netlink::notify_iface_protocol_change(
        netns,
        iface,
        removed_addresses,
        restored,
        Some(route_changes),
        removed_neighbors,
    );
    Ok(())
}

impl PreparedLinkAddressChange {
    pub(in crate::net) fn candidate(&self) -> &crate::net::route::FibTable {
        self.routes.candidate()
    }
}

#[derive(Debug, Clone, Copy)]
enum CommitMutation {
    Plain(AddressMutation),
}

struct AddressCandidates {
    before: Vec<IpCidr>,
    after: Vec<IpCidr>,
    metadata: Vec<AddressMetadata>,
}

/// Linux drops interface addresses when a link changes network namespace.
/// All fallible source/target preparation is complete before this point.
pub(in crate::net) fn detach_for_netns_move(iface: &Arc<dyn Iface>) {
    debug_assert!(iface.net_namespace().is_none());
    iface
        .smol_iface()
        .lock()
        .update_ip_addrs(|list| list.clear());
    iface.router_common().ip_addrs.write().clear();
    iface.common().address_metadata().lock().clear();
}

impl AddressCandidates {
    fn prepare(iface: &Arc<dyn Iface>, mutation: CommitMutation) -> Result<Self, SystemError> {
        let additional = usize::from(matches!(
            mutation,
            CommitMutation::Plain(AddressMutation::Add(_) | AddressMutation::Replace(_))
        ));
        let mirror = iface.router_common().ip_addrs.read();
        let before = try_clone_copy_slice(&mirror, 0)?;
        let after = try_clone_copy_slice(&mirror, additional)?;
        drop(mirror);
        let metadata = try_clone_metadata(&iface.common().address_metadata().lock(), additional)?;
        Ok(Self {
            before,
            after,
            metadata,
        })
    }
}

/// Mutates an address on an interface that is already visible in a netns.
pub(in crate::net) fn mutate_address(
    rtnl: &RtnlGuard,
    iface: &Arc<dyn Iface>,
    mutation: AddressMutation,
) -> Result<AddressMutationCommit, SystemError> {
    mutate_address_with_options(rtnl, iface, mutation, AddressOptions::default())
}

pub(in crate::net) fn mutate_address_with_options(
    rtnl: &RtnlGuard,
    iface: &Arc<dyn Iface>,
    mutation: AddressMutation,
    options: AddressOptions,
) -> Result<AddressMutationCommit, SystemError> {
    commit_published(rtnl, iface, CommitMutation::Plain(mutation), options)
}

pub(in crate::net) fn address_label(
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
) -> Result<CString, SystemError> {
    let explicit = iface
        .common()
        .address_metadata()
        .lock()
        .iter()
        .find(|entry| entry.cidr == cidr)
        .and_then(|entry| entry.label.clone());
    explicit
        .map(Ok)
        .unwrap_or_else(|| CString::new(iface.iface_name()).map_err(|_| SystemError::EINVAL))
}

/// Read one live RTNL dump item without cloning all configured addresses.
pub(in crate::net) fn address_at(
    iface: &Arc<dyn Iface>,
    index: usize,
) -> Result<Option<AddressInfo>, SystemError> {
    let smol_iface = iface.smol_iface().lock();
    let usable = smol_iface.ip_addrs();
    let metadata = iface.common().address_metadata().lock();
    let (cidr, entry) = if let Some(&cidr) = usable.get(index) {
        (cidr, metadata.iter().find(|entry| entry.cidr == cidr))
    } else {
        let Some(entry) = metadata
            .iter()
            .filter(|entry| !entry.state.is_usable())
            .nth(index - usable.len())
        else {
            return Ok(None);
        };
        (entry.cidr, Some(entry))
    };
    let label = match entry.and_then(|entry| entry.label.as_ref()) {
        Some(label) => try_clone_cstring(label)?,
        None => try_default_label(iface)?,
    };
    Ok(Some(AddressInfo {
        cidr,
        label,
        broadcast: entry.and_then(|entry| entry.broadcast),
        state: entry.map(|entry| entry.state).unwrap_or_default(),
        ipv6_nodad: entry.is_some_and(|entry| entry.ipv6_nodad),
        ipv6_permanent: entry.is_none_or(|entry| entry.ipv6_permanent),
        lifetimes: entry.and_then(|entry| entry.lifetimes),
    }))
}

pub(in crate::net) fn preferred_ipv6_deadlines(
    metadata: &[AddressMetadata],
) -> impl Iterator<Item = (Ipv6Address, i64)> + '_ {
    metadata.iter().filter_map(|entry| {
        if !entry.state.is_usable() {
            return None;
        }
        let IpAddress::Ipv6(address) = entry.cidr.address() else {
            return None;
        };
        Some((address, entry.lifetimes?.preferred_until_us()?))
    })
}

pub(in crate::net) fn publish_ipv6_preference(
    iface: &mut smoltcp::iface::Interface,
    metadata: &[AddressMetadata],
) {
    iface.set_ipv6_preferred_deadlines(
        preferred_ipv6_deadlines(metadata)
            .map(|(address, deadline)| (address, smoltcp::time::Instant::from_micros(deadline))),
    );
}

pub(in crate::net) fn address_info(
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
) -> Result<AddressInfo, SystemError> {
    let label = address_label(iface, cidr)?;
    let metadata = iface.common().address_metadata().lock();
    let entry = metadata.iter().find(|entry| entry.cidr == cidr);
    Ok(AddressInfo {
        cidr,
        label,
        broadcast: entry.and_then(|entry| entry.broadcast),
        state: entry.map(|entry| entry.state).unwrap_or_default(),
        ipv6_nodad: entry.is_some_and(|entry| entry.ipv6_nodad),
        ipv6_permanent: entry.is_none_or(|entry| entry.ipv6_permanent),
        lifetimes: entry.and_then(|entry| entry.lifetimes),
    })
}

pub(in crate::net) fn address_state(iface: &Arc<dyn Iface>, cidr: IpCidr) -> AddressState {
    iface
        .common()
        .address_metadata()
        .lock()
        .iter()
        .find(|entry| entry.cidr == cidr)
        .map(|entry| entry.state)
        .unwrap_or_default()
}

pub(in crate::net) fn has_address_object(iface: &Arc<dyn Iface>, cidr: IpCidr) -> bool {
    let smol_iface = iface.smol_iface().lock();
    smol_iface.ip_addrs().contains(&cidr)
        || iface
            .common()
            .address_metadata()
            .lock()
            .iter()
            .any(|entry| entry.cidr == cidr && !entry.state.is_usable())
}

/// A fully allocated IPv4 address-label rename.
///
/// Construction is fallible, but publication only moves owned state. Holding
/// RTNL across both phases keeps the address list and metadata generation
/// stable without teaching the address layer about SETLINK orchestration.
pub(in crate::net) struct PreparedAddressLabelRename {
    metadata: Vec<AddressMetadata>,
    renamed_ipv4: Vec<IpCidr>,
}

impl PreparedAddressLabelRename {
    /// Applies Linux's IPv4 alias-label rename rules to an owned candidate.
    pub(in crate::net) fn prepare(
        _rtnl: &RtnlGuard,
        iface: &Arc<dyn Iface>,
        new_name: &str,
    ) -> Result<Self, SystemError> {
        let metadata = try_clone_metadata(&iface.common().address_metadata().lock(), 0)?;
        let (metadata, renamed_ipv4) = prepare_renamed_metadata(iface, new_name, metadata)?;
        Ok(Self {
            metadata,
            renamed_ipv4,
        })
    }

    /// Publishes the prepared interface name and labels without allocation.
    pub(in crate::net) fn publish(self, iface: &Arc<dyn Iface>, new_name: String) -> Vec<IpCidr> {
        iface
            .common()
            .publish_name_and_address_metadata(new_name, self.metadata);
        self.renamed_ipv4
    }
}

fn prepare_renamed_metadata(
    iface: &Arc<dyn Iface>,
    new_name: &str,
    mut metadata: Vec<AddressMetadata>,
) -> Result<(Vec<AddressMetadata>, Vec<IpCidr>), SystemError> {
    const IFNAME_MAX: usize = 15;

    let ipv4_count = metadata
        .iter()
        .filter(|entry| matches!(entry.cidr, IpCidr::Ipv4(_)))
        .count();
    let mut renamed_ipv4 = Vec::new();
    renamed_ipv4
        .try_reserve_exact(ipv4_count)
        .map_err(|_| SystemError::ENOMEM)?;

    // Interface names are bounded by IFNAMSIZ. Copy the old name to the
    // stack so the name lock is never held across label allocation.
    let mut old_name = [0u8; IFNAME_MAX + 1];
    let old_name_len = iface.common().with_iface_name(|name| {
        let copied = name.len().min(old_name.len());
        old_name[..copied].copy_from_slice(&name.as_bytes()[..copied]);
        copied
    });
    let old_name = &old_name[..old_name_len];

    let mut ordinal = 0usize;
    for entry in metadata
        .iter_mut()
        .filter(|entry| matches!(entry.cidr, IpCidr::Ipv4(_)))
    {
        ordinal += 1;
        renamed_ipv4.push(entry.cidr);
        if ordinal == 1 {
            entry.label = None;
            continue;
        }

        let old_label = entry
            .label
            .as_ref()
            .map(|label| label.as_bytes())
            .unwrap_or(old_name);
        let mut generated_suffix = [0u8; 1 + 3 * core::mem::size_of::<usize>()];
        let suffix = match old_label.iter().position(|byte| *byte == b':') {
            Some(index) => &old_label[index..],
            None => decimal_alias_suffix(ordinal, &mut generated_suffix),
        };
        let suffix = if suffix.len() > IFNAME_MAX {
            &suffix[suffix.len() - IFNAME_MAX..]
        } else {
            suffix
        };
        let prefix_len = new_name.len().min(IFNAME_MAX - suffix.len());
        entry.label = Some(try_build_label(&new_name.as_bytes()[..prefix_len], suffix)?);
    }

    Ok((metadata, renamed_ipv4))
}

fn missing_loopback_ipv6_address(
    iface: &Arc<dyn Iface>,
    mtu: usize,
    addresses: &[IpCidr],
) -> Option<IpCidr> {
    let default = crate::driver::net::loopback::LOOPBACK_IPV6_CIDR;
    (mtu >= IPV6_MIN_MTU
        && iface.flags().contains(InterfaceFlags::LOOPBACK)
        && !addresses
            .iter()
            .any(|cidr| cidr.address() == default.address()))
    .then_some(default)
}

/// Prepare address changes caused by an MTU change or a loopback link-up.
pub(in crate::net) fn prepare_link_address_change(
    netns: &Arc<crate::process::namespace::net_namespace::NetNamespace>,
    iface: &Arc<dyn Iface>,
    mtu: Option<usize>,
    renamed_to: Option<&str>,
    was_up: bool,
    is_up: bool,
    staged_before: Option<&crate::net::route::FibTable>,
) -> Result<Option<PreparedLinkAddressChange>, SystemError> {
    let link_up = is_up && !was_up;
    let link_down = was_up && !is_up;
    let loopback = iface.flags().contains(InterfaceFlags::LOOPBACK);
    let effective_mtu = mtu.unwrap_or_else(|| iface.mtu());
    let restore_ipv6_addresses = link_up && effective_mtu >= IPV6_MIN_MTU;
    let withdraw_ipv6_routes =
        effective_mtu < IPV6_MIN_MTU && (mtu.is_some() || (loopback && link_up));
    let reset_ipv6_addresses = link_down || withdraw_ipv6_routes;
    if mtu.is_none() && !link_up && !link_down {
        return Ok(None);
    }
    let (has_ipv6_metadata, retained_count) = {
        let metadata = iface.common().address_metadata().lock();
        (
            reset_ipv6_addresses
                && metadata
                    .iter()
                    .any(|entry| matches!(entry.cidr, IpCidr::Ipv6(_))),
            if restore_ipv6_addresses {
                metadata
                    .iter()
                    .filter(|entry| matches!(entry.state, AddressState::Retained(_)))
                    .count()
            } else {
                0
            },
        )
    };
    let mirror = iface.router_common().ip_addrs.read();
    let remove_ipv6_addresses = reset_ipv6_addresses
        && (has_ipv6_metadata || mirror.iter().any(|cidr| matches!(cidr, IpCidr::Ipv6(_))));
    let restored = if loopback && restore_ipv6_addresses && iface.common().ipv6_enabled() {
        missing_loopback_ipv6_address(iface, effective_mtu, &mirror)
    } else {
        None
    };
    let mtu_removal = |cidr| {
        mtu.is_some_and(|mtu| match cidr {
            IpCidr::Ipv4(_) => mtu < IPV4_MIN_MTU,
            IpCidr::Ipv6(_) => mtu < IPV6_MIN_MTU,
        })
    };
    if !withdraw_ipv6_routes
        && !remove_ipv6_addresses
        && restored.is_none()
        && retained_count == 0
        && !mirror.iter().copied().any(mtu_removal)
    {
        return Ok(None);
    }
    let before = try_clone_copy_slice(&mirror, 0)?;
    let mut after =
        try_clone_copy_slice(&mirror, usize::from(restored.is_some()) + retained_count)?;
    drop(mirror);

    let mut metadata = try_clone_metadata(
        &iface.common().address_metadata().lock(),
        usize::from(restored.is_some()),
    )?;
    let keep_addr_on_down = netns.ipv6_all_keep_addr_on_down();
    let keep_addresses = remove_ipv6_addresses
        && iface.common().ipv6_enabled()
        && (loopback || effective_mtu >= IPV6_MIN_MTU)
        && if keep_addr_on_down != 0 {
            keep_addr_on_down > 0
        } else {
            ipv6_autoconf::Setting::KeepAddrOnDown.read(iface.common()) > 0
        };
    if keep_addresses {
        for entry in &mut metadata {
            let IpCidr::Ipv6(cidr) = entry.cidr else {
                continue;
            };
            let address = cidr.address();
            if !entry.ipv6_permanent || address.is_unicast_link_local() || address.is_loopback() {
                continue;
            }
            let state = match entry.state {
                AddressState::Retained(state)
                | AddressState::ManualDad(state @ (DadState::Tentative | DadState::DadFailed))
                | AddressState::AutoLinkLocal(
                    state @ (DadState::Tentative | DadState::DadFailed),
                )
                | AddressState::Slaac(state @ (DadState::Tentative | DadState::DadFailed)) => state,
                _ if entry.ipv6_nodad => DadState::Usable,
                _ => DadState::Tentative,
            };
            entry.state = AddressState::Retained(state);
        }
        after.retain(|cidr| {
            !metadata.iter().any(|entry| {
                entry.cidr == *cidr && matches!(entry.state, AddressState::Retained(_))
            })
        });
    }
    let (removed, deleted_addresses) = prepare_address_removal(
        iface,
        &mut after,
        &mut metadata,
        |cidr| mtu_removal(cidr) || (remove_ipv6_addresses && matches!(cidr, IpCidr::Ipv6(_))),
        keep_addresses,
    )?;

    metadata.extend(restored.map(AddressMetadata::configured));
    let (mut metadata, mut changed_addresses) = if let Some(new_name) = renamed_to {
        prepare_renamed_metadata(iface, new_name, metadata)?
    } else {
        (metadata, Vec::new())
    };
    changed_addresses
        .try_reserve(usize::from(restored.is_some()) + retained_count)
        .map_err(|_| SystemError::ENOMEM)?;
    if let Some(cidr) = restored {
        after.push(cidr);
        changed_addresses.push(cidr);
    }
    if restore_ipv6_addresses {
        let ethernet = iface.type_() == crate::driver::net::types::InterfaceType::ETHER;
        let now = crate::time::Instant::now().total_micros();
        for entry in &mut metadata {
            let AddressState::Retained(state) = entry.state else {
                continue;
            };
            if (state == DadState::Usable || entry.ipv6_nodad || !ethernet)
                && !entry
                    .lifetimes
                    .is_some_and(|lifetime| lifetime.expired(now))
            {
                entry.state = AddressState::Configured;
                after.push(entry.cidr);
                changed_addresses.push(entry.cidr);
            }
        }
    }
    let change = crate::net::route::AddressLinkChange {
        before: &before,
        after: &after,
        metadata,
        deleted_addresses: &deleted_addresses,
        ipv6_prefix_change: None,
        was_up,
        is_up,
        ipv6_enabled: None,
        withdraw_ipv6_routes,
    };
    let routes = match staged_before {
        Some(staged) => {
            crate::net::route::prepare_address_link_change_from(netns, iface, change, staged)?
        }
        None => crate::net::route::prepare_address_link_change(netns, iface, change)?,
    };
    Ok(Some(PreparedLinkAddressChange {
        routes,
        withdraws_ipv6: withdraw_ipv6_routes,
        removed,
        changed_addresses,
    }))
}

impl PreparedLinkAddressChange {
    pub(in crate::net) fn publish(
        self,
        netns: &Arc<crate::process::namespace::net_namespace::NetNamespace>,
        iface: &Arc<dyn Iface>,
        renamed_to: Option<String>,
        publish_mtu: impl FnOnce(),
        is_up: bool,
        publish_link_state: impl FnOnce(),
    ) -> (
        Vec<AddressInfo>,
        Vec<IpCidr>,
        crate::net::route::RouteNotifications,
    ) {
        let Self {
            routes,
            removed,
            changed_addresses,
            ..
        } = self;
        let route_changes = routes.publish_link_change(
            netns,
            iface,
            renamed_to,
            publish_mtu,
            publish_link_state,
            is_up,
        );
        (removed, changed_addresses, route_changes)
    }
}

fn try_default_label(iface: &Arc<dyn Iface>) -> Result<CString, SystemError> {
    iface.common().with_iface_name(|name| {
        let allocation_len = name.len().checked_add(1).ok_or(SystemError::ENOMEM)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(allocation_len)
            .map_err(|_| SystemError::ENOMEM)?;
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        CString::from_vec_with_nul(bytes).map_err(|_| SystemError::EINVAL)
    })
}

fn decimal_alias_suffix(mut ordinal: usize, storage: &mut [u8]) -> &[u8] {
    let mut cursor = storage.len();
    loop {
        cursor -= 1;
        storage[cursor] = b'0' + (ordinal % 10) as u8;
        ordinal /= 10;
        if ordinal == 0 {
            break;
        }
    }
    cursor -= 1;
    storage[cursor] = b':';
    &storage[cursor..]
}

fn try_build_label(prefix: &[u8], suffix: &[u8]) -> Result<CString, SystemError> {
    let payload_len = prefix
        .len()
        .checked_add(suffix.len())
        .ok_or(SystemError::ENOMEM)?;
    let allocation_len = payload_len.checked_add(1).ok_or(SystemError::ENOMEM)?;
    let mut label = Vec::new();
    label
        .try_reserve_exact(allocation_len)
        .map_err(|_| SystemError::ENOMEM)?;
    label.extend_from_slice(prefix);
    label.extend_from_slice(suffix);
    label.push(0);
    CString::from_vec_with_nul(label).map_err(|_| SystemError::EINVAL)
}

/// Initializes an address before an interface is published in a netns.
///
/// This narrow construction-time entry point exists so drivers do not grow a
/// second, weaker address mutation path. Published interfaces must use
/// [`mutate_address`] under RTNL.
pub(crate) fn initialize_address(iface: &Arc<dyn Iface>, cidr: IpCidr) -> Result<(), SystemError> {
    iface.common().with_unpublished(|| {
        commit_unpublished(
            iface,
            CommitMutation::Plain(AddressMutation::Add(cidr)),
            None,
            None,
        )
        .map(|_| ())
    })
}

fn commit_unpublished(
    iface: &Arc<dyn Iface>,
    mutation: CommitMutation,
    requested_label: Option<CString>,
    requested_broadcast: Option<Ipv4Address>,
) -> Result<AddressMutationOutcome, SystemError> {
    validate_mutation(mutation)?;

    let mut smol_iface = iface.smol_iface().lock();
    let mut metadata = iface.common().address_metadata().lock();
    let may_add = matches!(
        mutation,
        CommitMutation::Plain(AddressMutation::Add(_) | AddressMutation::Replace(_))
    );
    if may_add {
        metadata.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
    }
    // Keep the established smoltcp -> metadata -> router projection order and
    // reserve every growable publication target before changing any state.
    let projected_len = smol_iface
        .ip_addrs()
        .len()
        .checked_add(usize::from(may_add))
        .ok_or(SystemError::ENOMEM)?;
    let mut projection = iface.router_common().ip_addrs.write();
    let additional_projection_capacity = projected_len.saturating_sub(projection.len());
    projection
        .try_reserve(additional_projection_capacity)
        .map_err(|_| SystemError::ENOMEM)?;
    let outcome = match mutation {
        CommitMutation::Plain(AddressMutation::Add(cidr)) => {
            let outcome = add(&mut smol_iface, cidr)?;
            metadata.push(AddressMetadata {
                cidr,
                label: requested_label,
                broadcast: requested_broadcast,
                state: AddressState::Configured,
                ipv6_nodad: false,
                ipv6_permanent: true,
                lifetimes: None,
            });
            outcome
        }
        CommitMutation::Plain(AddressMutation::Delete(cidr)) => {
            let outcome = delete(&mut smol_iface, cidr)?;
            metadata.retain(|entry| entry.cidr != cidr);
            outcome
        }
        CommitMutation::Plain(AddressMutation::Replace(cidr)) => {
            let outcome = replace(&mut smol_iface, cidr)?;
            if matches!(outcome, AddressMutationOutcome::Added(_)) {
                metadata.push(AddressMetadata {
                    cidr,
                    label: requested_label,
                    broadcast: requested_broadcast,
                    state: AddressState::Configured,
                    ipv6_nodad: false,
                    ipv6_permanent: true,
                    lifetimes: None,
                });
            }
            outcome
        }
    };

    projection.clear();
    projection.extend_from_slice(smol_iface.ip_addrs());

    Ok(outcome)
}

fn commit_published(
    _rtnl: &RtnlGuard,
    iface: &Arc<dyn Iface>,
    mutation: CommitMutation,
    options: AddressOptions,
) -> Result<AddressMutationCommit, SystemError> {
    validate_mutation(mutation)?;
    let netns = iface.net_namespace().ok_or(SystemError::ENODEV)?;
    let cancel_dad = match mutation {
        CommitMutation::Plain(AddressMutation::Delete(cidr)) => match cidr.address() {
            IpAddress::Ipv6(target) if address_state(iface, cidr).has_dad_state() => Some(target),
            _ => None,
        },
        _ => None,
    };
    let AddressCandidates {
        before,
        mut after,
        mut metadata,
    } = AddressCandidates::prepare(iface, mutation)?;
    let existing = |requested| {
        after
            .iter()
            .copied()
            .chain(
                metadata
                    .iter()
                    .filter(|entry| !entry.state.is_usable())
                    .map(|entry| entry.cidr),
            )
            .find(|configured| same_new_or_replace_identity(*configured, requested))
    };

    let mut lifetime_deadline_changed = false;
    let mut manual_probe = None;
    let mut deleted_retained = false;
    let mut ipv6_prefix_change = None;
    let outcome = match mutation {
        CommitMutation::Plain(AddressMutation::Add(cidr) | AddressMutation::Replace(cidr)) => {
            let existing = existing(cidr);
            if existing.is_some()
                && matches!(mutation, CommitMutation::Plain(AddressMutation::Add(_)))
            {
                return Err(SystemError::EEXIST);
            }
            let ipv6_lifetimes = options
                .ipv6_lifetimes
                .map(ipv6_lifetime::ConfiguredLifetimes::into_deadlines)
                .transpose()?;
            let ipv6_permanent =
                ipv6_lifetimes.is_none_or(ipv6_lifetime::Lifetimes::valid_indefinitely);
            lifetime_deadline_changed = ipv6_lifetimes
                .and_then(ipv6_lifetime::Lifetimes::next_deadline)
                .is_some();
            if let Some(existing) = existing {
                let index = metadata
                    .iter()
                    .position(|entry| entry.cidr == existing)
                    .unwrap_or_else(|| {
                        metadata.push(AddressMetadata::configured(existing));
                        metadata.len() - 1
                    });
                let entry = &mut metadata[index];
                if let IpCidr::Ipv6(address) = existing {
                    ipv6_prefix_change = Some(Ipv6PrefixChange::Upsert {
                        address,
                        expires_at_us: ipv6_lifetimes
                            .and_then(ipv6_lifetime::Lifetimes::valid_until_us),
                        replace_permanent: entry.ipv6_permanent,
                    });
                }
                match entry.state {
                    AddressState::Slaac(DadState::Usable) => entry.state = AddressState::Configured,
                    AddressState::Slaac(state) => entry.state = AddressState::ManualDad(state),
                    _ => {}
                }
                entry.lifetimes = ipv6_lifetimes.map(|updated| updated.replacing(entry.lifetimes));
                entry.ipv6_permanent = ipv6_permanent;
                if let Some(nodad) = options.ipv6_nodad {
                    entry.ipv6_nodad = nodad;
                }
                AddressMutationOutcome::Replaced(existing)
            } else {
                validate_published_address_addition(iface, cidr)?;
                if let IpCidr::Ipv6(address) = cidr {
                    ipv6_prefix_change = Some(Ipv6PrefixChange::Upsert {
                        address,
                        expires_at_us: ipv6_lifetimes
                            .and_then(ipv6_lifetime::Lifetimes::valid_until_us),
                        replace_permanent: false,
                    });
                }
                if let (IpCidr::Ipv6(ipv6), Some(false)) = (cidr, options.ipv6_nodad) {
                    if ipv6.address().x_is_unicast() {
                        manual_probe = ipv6_autoconf::manual_dad_transmits(iface.as_ref())
                            .map(|transmits| (ipv6, transmits));
                    }
                }
                let state = if manual_probe.is_some() {
                    AddressState::ManualDad(DadState::Tentative)
                } else {
                    insert_address_candidate(&mut after, cidr);
                    AddressState::Configured
                };
                metadata.push(AddressMetadata {
                    cidr,
                    label: options.label,
                    broadcast: options.broadcast,
                    state,
                    ipv6_nodad: options.ipv6_nodad.unwrap_or(false),
                    ipv6_permanent,
                    lifetimes: ipv6_lifetimes,
                });
                AddressMutationOutcome::Added(cidr)
            }
        }
        CommitMutation::Plain(AddressMutation::Delete(cidr)) => {
            if let Some(index) = find_for_delete(&after, cidr) {
                after.remove(index);
            } else if !metadata
                .iter()
                .any(|entry| entry.cidr == cidr && !entry.state.is_usable())
            {
                return Err(SystemError::EADDRNOTAVAIL);
            }
            if let IpCidr::Ipv6(address) = cidr {
                if metadata
                    .iter()
                    .find(|entry| entry.cidr == cidr)
                    .is_none_or(|entry| entry.ipv6_permanent)
                {
                    ipv6_prefix_change = Some(Ipv6PrefixChange::RemovePermanent(address));
                }
            }
            metadata.retain(|entry| {
                if entry.cidr != cidr {
                    return true;
                }
                deleted_retained = matches!(entry.state, AddressState::Retained(_));
                false
            });
            AddressMutationOutcome::Deleted(cidr)
        }
    };
    let deleted = match outcome {
        AddressMutationOutcome::Deleted(cidr) if before.contains(&cidr) || deleted_retained => {
            Some(cidr.address())
        }
        _ => None,
    };
    let is_up = iface.flags().contains(InterfaceFlags::UP);
    let prepared = crate::net::route::prepare_address_link_change(
        &netns,
        iface,
        crate::net::route::AddressLinkChange {
            before: &before,
            after: &after,
            metadata,
            deleted_addresses: deleted.as_slice(),
            ipv6_prefix_change,
            was_up: is_up,
            is_up,
            ipv6_enabled: None,
            withdraw_ipv6_routes: false,
        },
    )?;
    if let Some((address, transmits)) = manual_probe {
        ipv6_autoconf::start_manual_probe(&netns, iface.common(), address, transmits)?;
    }
    let route_changes = prepared.publish(&netns, iface);
    if ipv6_prefix_change.is_some() && is_up {
        netns.notify_ipv6_route_deadline();
    }
    if manual_probe.is_some() || lifetime_deadline_changed {
        ipv6_autoconf::changed(iface.common());
    }
    if let Some(target) = cancel_dad {
        ipv6_autoconf::cancel_target(iface.common(), target);
    }
    Ok(AddressMutationCommit {
        outcome,
        route_changes,
    })
}

fn prepare_automatic_address(
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
    state: AddressState,
    lifetimes: Option<ipv6_lifetime::Lifetimes>,
) -> Result<crate::net::route::PreparedAddressRouteCommit, SystemError> {
    let AddressCandidates {
        before,
        mut after,
        mut metadata,
    } = AddressCandidates::prepare(iface, CommitMutation::Plain(AddressMutation::Add(cidr)))?;
    if let Some(entry) = metadata.iter_mut().find(|entry| entry.cidr == cidr) {
        if !matches!(
            (entry.state, state),
            (
                AddressState::AutoLinkLocal(_),
                AddressState::AutoLinkLocal(_)
            ) | (AddressState::Slaac(_), AddressState::Slaac(_))
                | (
                    AddressState::ManualDad(_) | AddressState::Retained(_),
                    AddressState::ManualDad(_) | AddressState::Configured
                )
        ) {
            return Err(SystemError::EEXIST);
        }
        entry.state = state;
        entry.lifetimes = lifetimes;
    } else {
        metadata.push(AddressMetadata {
            cidr,
            label: None,
            broadcast: None,
            state,
            ipv6_nodad: false,
            ipv6_permanent: !matches!(state, AddressState::Slaac(_)),
            lifetimes,
        });
    }
    if state.is_usable() && !after.contains(&cidr) {
        insert_address_candidate(&mut after, cidr);
    }
    let up = iface.flags().contains(InterfaceFlags::UP);
    crate::net::route::prepare_address_link_change(
        netns,
        iface,
        crate::net::route::AddressLinkChange {
            before: &before,
            after: &after,
            metadata,
            deleted_addresses: &[],
            ipv6_prefix_change: None,
            was_up: up,
            is_up: up,
            ipv6_enabled: None,
            withdraw_ipv6_routes: false,
        },
    )
}

fn validate_published_address_addition(
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
) -> Result<(), SystemError> {
    if matches!(cidr, IpCidr::Ipv6(_)) && !iface.common().ipv6_enabled() {
        return Err(SystemError::EACCES);
    }
    // Linux destroys the IPv4 in_device once MTU drops below the protocol
    // minimum. A later address add cannot recreate it until the MTU is valid.
    // IPv6 differs: Linux permits an explicit add below 1280, so keep this
    // admission rule IPv4-specific.
    if matches!(cidr, IpCidr::Ipv4(_)) && iface.mtu() < IPV4_MIN_MTU {
        return Err(SystemError::ENOBUFS);
    }
    Ok(())
}

fn insert_address_candidate(addresses: &mut Vec<IpCidr>, cidr: IpCidr) {
    debug_assert!(addresses.len() < addresses.capacity());
    let index = match cidr.address() {
        IpAddress::Ipv4(_) => addresses
            .iter()
            .position(|item| matches!(item.address(), IpAddress::Ipv6(_)))
            .unwrap_or(addresses.len()),
        IpAddress::Ipv6(_) => addresses.len(),
    };
    addresses.insert(index, cidr);
}

fn try_clone_copy_slice<T: Copy>(source: &[T], additional: usize) -> Result<Vec<T>, SystemError> {
    let capacity = source
        .len()
        .checked_add(additional)
        .ok_or(SystemError::ENOMEM)?;
    let mut result = Vec::new();
    result
        .try_reserve_exact(capacity)
        .map_err(|_| SystemError::ENOMEM)?;
    result.extend_from_slice(source);
    Ok(result)
}

fn try_clone_metadata(
    source: &[AddressMetadata],
    additional: usize,
) -> Result<Vec<AddressMetadata>, SystemError> {
    let capacity = source
        .len()
        .checked_add(additional)
        .ok_or(SystemError::ENOMEM)?;
    let mut result = Vec::new();
    result
        .try_reserve_exact(capacity)
        .map_err(|_| SystemError::ENOMEM)?;
    for entry in source {
        let label = entry.label.as_ref().map(try_clone_cstring).transpose()?;
        result.push(AddressMetadata {
            cidr: entry.cidr,
            label,
            broadcast: entry.broadcast,
            state: entry.state,
            ipv6_nodad: entry.ipv6_nodad,
            ipv6_permanent: entry.ipv6_permanent,
            lifetimes: entry.lifetimes,
        });
    }
    Ok(result)
}

fn try_clone_cstring(source: &CString) -> Result<CString, SystemError> {
    let bytes = source.as_bytes_with_nul();
    let mut copy = Vec::new();
    copy.try_reserve_exact(bytes.len())
        .map_err(|_| SystemError::ENOMEM)?;
    copy.extend_from_slice(bytes);
    CString::from_vec_with_nul(copy).map_err(|_| SystemError::EINVAL)
}

fn validate_mutation(mutation: CommitMutation) -> Result<(), SystemError> {
    match mutation {
        CommitMutation::Plain(AddressMutation::Add(cidr))
        | CommitMutation::Plain(AddressMutation::Delete(cidr))
        | CommitMutation::Plain(AddressMutation::Replace(cidr)) => validate_cidr(cidr),
    }
}

fn validate_cidr(cidr: IpCidr) -> Result<(), SystemError> {
    // smoltcp asserts on multicast/broadcast addresses in update_ip_addrs().
    // Its unspecified address is not a configured interface object either;
    // the IPv4 rtnetlink no-op compatibility case is handled by the adapter.
    if !cidr.address().is_unicast() {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

fn add(
    iface: &mut smoltcp::iface::Interface,
    cidr: IpCidr,
) -> Result<AddressMutationOutcome, SystemError> {
    if find_for_new_or_replace(iface.ip_addrs(), cidr).is_some() {
        return Err(SystemError::EEXIST);
    }

    let mut inserted = false;
    iface.update_ip_addrs(|addresses| {
        let index = match cidr.address() {
            IpAddress::Ipv4(_) => addresses
                .iter()
                .position(|item| matches!(item.address(), IpAddress::Ipv6(_)))
                .unwrap_or(addresses.len()),
            IpAddress::Ipv6(_) => addresses.len(),
        };
        inserted = addresses.insert(index, cidr).is_ok();
    });
    if !inserted {
        return Err(SystemError::ENOSPC);
    }

    Ok(AddressMutationOutcome::Added(cidr))
}

fn delete(
    iface: &mut smoltcp::iface::Interface,
    cidr: IpCidr,
) -> Result<AddressMutationOutcome, SystemError> {
    let index = find_for_delete(iface.ip_addrs(), cidr).ok_or(SystemError::EADDRNOTAVAIL)?;
    let effective = iface.ip_addrs()[index];
    iface.update_ip_addrs(|addresses| {
        addresses.remove(index);
    });

    Ok(AddressMutationOutcome::Deleted(effective))
}

fn replace(
    iface: &mut smoltcp::iface::Interface,
    cidr: IpCidr,
) -> Result<AddressMutationOutcome, SystemError> {
    let Some(index) = find_for_new_or_replace(iface.ip_addrs(), cidr) else {
        return add(iface, cidr);
    };
    let effective = iface.ip_addrs()[index];
    Ok(AddressMutationOutcome::Replaced(effective))
}

fn find_for_new_or_replace(addresses: &[IpCidr], requested: IpCidr) -> Option<usize> {
    addresses
        .iter()
        .position(|configured| same_new_or_replace_identity(*configured, requested))
}

fn find_for_delete(addresses: &[IpCidr], requested: IpCidr) -> Option<usize> {
    addresses
        .iter()
        .position(|configured| *configured == requested)
}

fn same_new_or_replace_identity(configured: IpCidr, requested: IpCidr) -> bool {
    match (configured.address(), requested.address()) {
        (IpAddress::Ipv4(_), IpAddress::Ipv4(_)) => configured == requested,
        (IpAddress::Ipv6(configured), IpAddress::Ipv6(requested)) => configured == requested,
        _ => false,
    }
}

/// Returns whether `address` is configured on `iface`.
///
/// Keep address-ownership queries next to the authoritative address mutation
/// core so route validation and socket source selection cannot grow subtly
/// different notions of a local address.
pub(crate) fn iface_has_address(iface: &Arc<dyn Iface>, address: IpAddress) -> bool {
    iface
        .common()
        .ip_addrs()
        .iter()
        .any(|cidr| cidr.address() == address)
}

/// Returns whether Linux would treat `address` as local on `iface` for bind
/// and fixed-source output validation.
///
/// Most addresses require an exact configured entry. IPv4 loopback devices
/// additionally own every address in each configured prefix; this is the
/// shared rule used by socket placement and route-source validation.
pub(crate) fn iface_accepts_local_address(iface: &Arc<dyn Iface>, address: IpAddress) -> bool {
    let addresses = iface.common().ip_addrs();
    iface_accepts_local_address_from(iface, &addresses, address)
}

fn retained_ipv6_source(entry: &AddressMetadata) -> Option<Ipv6Cidr> {
    match (entry.cidr, entry.state) {
        (IpCidr::Ipv6(cidr), AddressState::Retained(DadState::Usable)) => Some(cidr),
        _ => None,
    }
}

/// Retained usable addresses remain available to sockets while the device is down.
pub(crate) fn iface_retains_ipv6_source(iface: &Arc<dyn Iface>, address: Ipv6Address) -> bool {
    iface
        .common()
        .address_metadata()
        .lock()
        .iter()
        .filter_map(retained_ipv6_source)
        .any(|cidr| cidr.address() == address)
}

/// Rank an interface's active and retained usable sources as one candidate set.
pub(super) fn ipv6_send_source_candidate(
    iface: &Arc<dyn Iface>,
    destination: &Ipv6Address,
    now: smoltcp::time::Instant,
    on_route_device: Option<bool>,
) -> Option<(impl Ord, Ipv6Address)> {
    // Read both views under the address publication lock order.
    let smol = iface.smol_iface().lock();
    let metadata = iface.common().address_metadata().lock();
    let mut retained = metadata.iter().filter_map(|entry| {
        let cidr = retained_ipv6_source(entry)?;
        let deprecated = entry
            .lifetimes
            .is_some_and(|lifetimes| lifetimes.deprecated(now.total_micros()));
        Some((cidr, deprecated))
    });
    smol.ipv6_source_candidate_at(destination, now, on_route_device, Some(&mut retained))
}

/// Returns whether `address` is an IPv4 broadcast address on `iface`.
///
/// This covers both the limited broadcast address and subnet-directed
/// broadcasts derived from the interface's configured prefixes.
pub(crate) fn iface_accepts_broadcast_address(iface: &Arc<dyn Iface>, address: IpAddress) -> bool {
    let IpAddress::Ipv4(address) = address else {
        return false;
    };
    address.is_broadcast()
        || iface.common().has_explicit_ipv4_broadcast(address)
        || iface.common().ip_addrs().iter().any(|cidr| match cidr {
            IpCidr::Ipv4(cidr) => cidr
                .broadcast()
                .is_some_and(|broadcast| broadcast == address),
            IpCidr::Ipv6(_) => false,
        })
}

pub(crate) fn netns_accepts_broadcast_address(
    netns: &Arc<NetNamespace>,
    address: IpAddress,
) -> bool {
    matches!(address, IpAddress::Ipv4(address) if address.is_broadcast())
        || netns
            .device_list()
            .values()
            .any(|iface| iface_accepts_broadcast_address(iface, address))
}

/// Whether any interface in this namespace needs the IPv4 FIB to classify
/// explicit broadcasts on ingress. A frame may arrive on a different device
/// from the broadcast route's owner (Linux weak-host local delivery).
pub(crate) fn configured_ipv4_broadcast_count(metadata: &[AddressMetadata]) -> usize {
    metadata
        .iter()
        .filter(|entry| {
            entry
                .broadcast
                .is_some_and(|address| !address.is_unspecified() && !address.is_broadcast())
        })
        .count()
}

fn iface_accepts_local_address_from(
    iface: &Arc<dyn Iface>,
    addresses: &[IpCidr],
    address: IpAddress,
) -> bool {
    if addresses.iter().any(|cidr| cidr.address() == address) {
        return true;
    }
    let IpAddress::Ipv4(address) = address else {
        return false;
    };
    iface.flags().contains(InterfaceFlags::LOOPBACK)
        && addresses.iter().any(|cidr| match cidr {
            IpCidr::Ipv4(cidr) => cidr.contains_addr(&address),
            IpCidr::Ipv6(_) => false,
        })
}

/// Selects the IPv4 source that Linux's `inet_select_addr()` would use for an
/// output route on this interface.
///
/// A gateway is the only subnet-selection hint used by IPv4 FIB output. For a
/// direct route Linux passes an unspecified `nh_gw4`, so the first configured
/// IPv4 address remains the fallback instead of matching the final remote.
pub(crate) fn select_ipv4_source_address(
    iface: &Arc<dyn Iface>,
    gateway: Option<Ipv4Address>,
) -> Option<IpAddress> {
    select_ipv4_source_from(iface.common().ip_addrs().as_slice(), gateway)
}

fn select_ipv4_source_from(
    addresses: &[IpCidr],
    gateway: Option<Ipv4Address>,
) -> Option<IpAddress> {
    let mut fallback = None;
    for cidr in addresses {
        let IpCidr::Ipv4(cidr) = cidr else {
            continue;
        };
        fallback.get_or_insert(IpAddress::Ipv4(cidr.address()));
        if gateway.is_some_and(|gateway| cidr.contains_addr(&gateway)) {
            return Some(IpAddress::Ipv4(cidr.address()));
        }
    }
    fallback
}

/// Returns whether `address` is configured anywhere in `netns`.
pub(crate) fn netns_has_address(
    netns: &Arc<crate::process::namespace::net_namespace::NetNamespace>,
    address: IpAddress,
) -> bool {
    netns
        .device_list()
        .values()
        .any(|iface| iface_has_address(iface, address))
}

/// Applies source-address ownership supported by DragonOS's transport layout.
/// IPv4 can use weak-host ownership because routed local delivery transfers
/// packets to the address owner's SocketSet. IPv6 does not yet have that
/// namespace-level delivery path, so every IPv6 source must belong to the
/// egress interface.
pub(crate) fn source_address_usable_on_iface(
    netns: &Arc<crate::process::namespace::net_namespace::NetNamespace>,
    iface: &Arc<dyn Iface>,
    address: IpAddress,
) -> bool {
    if source_address_requires_iface(address) {
        iface_has_address(iface, address)
    } else {
        netns
            .device_list()
            .values()
            .any(|iface| iface_accepts_local_address(iface, address))
    }
}

/// Read-only address ownership view for a not-yet-published interface change.
///
/// Address deletion prepares the replacement FIB before publishing the new
/// interface address list. This view substitutes that candidate list while
/// using one namespace snapshot for every FIB entry, avoiding nested device
/// list locking and keeping family/scope policy centralized.
pub(crate) struct CandidateAddressOwnership<'a> {
    devices: &'a alloc::collections::BTreeMap<usize, Arc<dyn Iface>>,
    changed_iface: &'a Arc<dyn Iface>,
    changed_addresses: &'a [IpCidr],
}

impl<'a> CandidateAddressOwnership<'a> {
    pub(crate) fn new(
        devices: &'a alloc::collections::BTreeMap<usize, Arc<dyn Iface>>,
        changed_iface: &'a Arc<dyn Iface>,
        changed_addresses: &'a [IpCidr],
    ) -> Self {
        Self {
            devices,
            changed_iface,
            changed_addresses,
        }
    }

    fn iface_accepts_local_address(&self, iface: &Arc<dyn Iface>, address: IpAddress) -> bool {
        let addresses = if Arc::ptr_eq(iface, self.changed_iface) {
            self.changed_addresses
        } else {
            return iface_accepts_local_address(iface, address);
        };
        iface_accepts_local_address_from(iface, addresses, address)
    }

    pub(crate) fn source_usable_on_oif(
        &self,
        oif: u32,
        address: IpAddress,
        require_iface: bool,
    ) -> bool {
        let Some(egress_iface) = self.devices.get(&(oif as usize)) else {
            return false;
        };
        if require_iface {
            self.iface_accepts_local_address(egress_iface, address)
        } else {
            self.devices
                .values()
                .any(|iface| self.iface_accepts_local_address(iface, address))
        }
    }
}

fn source_address_requires_iface(address: IpAddress) -> bool {
    matches!(address, IpAddress::Ipv6(_))
}
