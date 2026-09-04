//! Protocol-independent interface address mutation core.
//!
//! Runtime callers must hold RTNL for the whole operation.  The smoltcp
//! address list, its connected-route projection, and the router compatibility
//! projection are committed under one smoltcp interface lock.

use alloc::{ffi::CString, string::String, sync::Arc, vec::Vec};

use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address};
use system_error::SystemError;

use crate::{
    driver::net::{AddressMetadata, Iface},
    net::rtnl::RtnlGuard,
};

const IPV4_MIN_MTU: usize = 68;
const IPV6_MIN_MTU: usize = 1280;

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

pub(crate) struct RemovedAddress {
    pub cidr: IpCidr,
    pub label: CString,
}

pub(in crate::net) struct PreparedMtuAddressChange {
    routes: crate::net::route::PreparedAddressRouteCommit,
    removed: Vec<RemovedAddress>,
    renamed_ipv4: Vec<IpCidr>,
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
    commit_published(rtnl, iface, CommitMutation::Plain(mutation), None)
}

pub(in crate::net) fn mutate_labeled_address(
    rtnl: &RtnlGuard,
    iface: &Arc<dyn Iface>,
    mutation: AddressMutation,
    label: Option<CString>,
) -> Result<AddressMutationCommit, SystemError> {
    commit_published(rtnl, iface, CommitMutation::Plain(mutation), label)
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

pub(in crate::net) fn address_snapshot(
    iface: &Arc<dyn Iface>,
) -> Result<Vec<(IpCidr, CString)>, SystemError> {
    let smol_iface = iface.smol_iface().lock();
    let metadata = iface.common().address_metadata().lock();
    let default_label = CString::new(iface.iface_name()).map_err(|_| SystemError::EINVAL)?;
    smol_iface
        .ip_addrs()
        .iter()
        .map(|cidr| {
            let label = metadata
                .iter()
                .find(|entry| entry.cidr == *cidr)
                .and_then(|entry| entry.label.clone())
                .unwrap_or_else(|| default_label.clone());
            Ok((*cidr, label))
        })
        .collect()
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

pub(in crate::net) fn prepare_mtu_address_change(
    _rtnl: &RtnlGuard,
    netns: &Arc<crate::process::namespace::net_namespace::NetNamespace>,
    iface: &Arc<dyn Iface>,
    mtu: usize,
    renamed_to: Option<&str>,
    was_up: bool,
    is_up: bool,
) -> Result<Option<PreparedMtuAddressChange>, SystemError> {
    let mirror = iface.router_common().ip_addrs.read();
    let before = try_clone_copy_slice(&mirror, 0)?;
    let mut after = try_clone_copy_slice(&mirror, 0)?;
    drop(mirror);

    let removal_count = after
        .iter()
        .filter(|cidr| match cidr {
            IpCidr::Ipv4(_) => mtu < IPV4_MIN_MTU,
            IpCidr::Ipv6(_) => mtu < IPV6_MIN_MTU,
        })
        .count();
    if removal_count == 0 {
        return Ok(None);
    }

    let mut metadata = try_clone_metadata(&iface.common().address_metadata().lock(), 0)?;
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
        let remove = match cidr {
            IpCidr::Ipv4(_) => mtu < IPV4_MIN_MTU,
            IpCidr::Ipv6(_) => mtu < IPV6_MIN_MTU,
        };
        if !remove {
            index += 1;
            continue;
        }
        after.remove(index);
        deleted_addresses.push(cidr.address());
        let label =
            if let Some(metadata_index) = metadata.iter().position(|entry| entry.cidr == cidr) {
                let entry = metadata.remove(metadata_index);
                match entry.label {
                    Some(label) => label,
                    None => try_clone_cstring(&default_label)?,
                }
            } else {
                try_clone_cstring(&default_label)?
            };
        removed.push(RemovedAddress { cidr, label });
    }

    let (metadata, renamed_ipv4) = if let Some(new_name) = renamed_to {
        prepare_renamed_metadata(iface, new_name, metadata)?
    } else {
        (metadata, Vec::new())
    };
    let routes = crate::net::route::prepare_address_link_change(
        netns,
        iface,
        crate::net::route::AddressLinkChange {
            before: &before,
            after: &after,
            metadata,
            deleted_addresses: &deleted_addresses,
            was_up,
            is_up,
        },
    )?;
    Ok(Some(PreparedMtuAddressChange {
        routes,
        removed,
        renamed_ipv4,
    }))
}

impl PreparedMtuAddressChange {
    pub(in crate::net) fn publish(
        self,
        netns: &Arc<crate::process::namespace::net_namespace::NetNamespace>,
        iface: &Arc<dyn Iface>,
        renamed_to: Option<String>,
        publish_mtu: impl FnOnce(),
        is_up: bool,
        publish_link_state: impl FnOnce(),
    ) -> (
        Vec<RemovedAddress>,
        Vec<IpCidr>,
        crate::net::route::RouteNotifications,
    ) {
        let Self {
            routes,
            removed,
            renamed_ipv4,
        } = self;
        let route_changes = routes.publish_link_change(
            netns,
            iface,
            renamed_to,
            publish_mtu,
            publish_link_state,
            is_up,
        );
        (removed, renamed_ipv4, route_changes)
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
        )
        .map(|_| ())
    })
}

fn commit_unpublished(
    iface: &Arc<dyn Iface>,
    mutation: CommitMutation,
    requested_label: Option<CString>,
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
    rtnl: &RtnlGuard,
    iface: &Arc<dyn Iface>,
    mutation: CommitMutation,
    requested_label: Option<CString>,
) -> Result<AddressMutationCommit, SystemError> {
    validate_mutation(mutation)?;
    let netns = iface.net_namespace().ok_or(SystemError::ENODEV)?;
    let AddressCandidates {
        before,
        mut after,
        mut metadata,
    } = AddressCandidates::prepare(iface, mutation)?;

    let outcome = match mutation {
        CommitMutation::Plain(AddressMutation::Add(cidr)) => {
            if find_for_new_or_replace(&after, cidr).is_some() {
                return Err(SystemError::EEXIST);
            }
            validate_published_address_addition(iface, cidr)?;
            insert_address_candidate(&mut after, cidr);
            metadata.push(AddressMetadata {
                cidr,
                label: requested_label,
            });
            AddressMutationOutcome::Added(cidr)
        }
        CommitMutation::Plain(AddressMutation::Delete(cidr)) => {
            let index = find_for_delete(&after, cidr).ok_or(SystemError::EADDRNOTAVAIL)?;
            let effective = after.remove(index);
            metadata.retain(|entry| entry.cidr != effective);
            AddressMutationOutcome::Deleted(effective)
        }
        CommitMutation::Plain(AddressMutation::Replace(cidr)) => {
            if let Some(index) = find_for_new_or_replace(&after, cidr) {
                AddressMutationOutcome::Replaced(after[index])
            } else {
                validate_published_address_addition(iface, cidr)?;
                insert_address_candidate(&mut after, cidr);
                metadata.push(AddressMetadata {
                    cidr,
                    label: requested_label,
                });
                AddressMutationOutcome::Added(cidr)
            }
        }
    };
    let deleted = match outcome {
        AddressMutationOutcome::Deleted(cidr) => Some(cidr.address()),
        _ => None,
    };
    let route_changes = crate::net::route::commit_addresses(
        rtnl, &netns, iface, &before, &after, metadata, deleted,
    )?;
    Ok(AddressMutationCommit {
        outcome,
        route_changes,
    })
}

fn validate_published_address_addition(
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
) -> Result<(), SystemError> {
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
        netns_has_address(netns, address)
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

    fn iface_has_address(&self, iface: &Arc<dyn Iface>, address: IpAddress) -> bool {
        if Arc::ptr_eq(iface, self.changed_iface) {
            self.changed_addresses
                .iter()
                .any(|cidr| cidr.address() == address)
        } else {
            iface_has_address(iface, address)
        }
    }

    pub(crate) fn source_usable_on_oif(&self, oif: u32, address: IpAddress) -> bool {
        let Some(egress_iface) = self.devices.get(&(oif as usize)) else {
            return false;
        };
        if source_address_requires_iface(address) {
            self.iface_has_address(egress_iface, address)
        } else {
            self.devices
                .values()
                .any(|iface| self.iface_has_address(iface, address))
        }
    }
}

fn source_address_requires_iface(address: IpAddress) -> bool {
    matches!(address, IpAddress::Ipv6(_))
}
