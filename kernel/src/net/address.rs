//! Protocol-independent interface address mutation core.
//!
//! Runtime callers must hold RTNL for the whole operation.  The smoltcp
//! address list, its connected-route projection, and the router compatibility
//! projection are committed under one smoltcp interface lock.

use alloc::{ffi::CString, format, sync::Arc, vec::Vec};

use smoltcp::wire::{IpAddress, IpCidr};
use system_error::SystemError;

use crate::{
    driver::net::{AddressMetadata, Iface},
    net::rtnl::RtnlGuard,
};

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
    pub route_changes: crate::net::route::RouteChanges,
}

#[derive(Debug, Clone, Copy)]
enum CommitMutation {
    Plain(AddressMutation),
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

/// Applies Linux's IPv4 alias-label rename rules while RTNL is held.
pub(in crate::net) fn rename_address_labels(
    _rtnl: &RtnlGuard,
    iface: &Arc<dyn Iface>,
    new_name: &str,
) -> Result<Vec<IpCidr>, SystemError> {
    const IFNAME_MAX: usize = 15;

    let old_name = iface.iface_name();
    let mut metadata = iface.common().address_metadata().lock();
    let mut renamed = Vec::new();
    let mut ordinal = 0usize;
    for entry in metadata
        .iter_mut()
        .filter(|entry| matches!(entry.cidr, IpCidr::Ipv4(_)))
    {
        ordinal += 1;
        renamed.push(entry.cidr);
        if ordinal == 1 {
            entry.label = None;
            continue;
        }

        let old_label = entry
            .label
            .as_ref()
            .map(|label| label.as_bytes())
            .unwrap_or_else(|| old_name.as_bytes());
        let generated_suffix;
        let suffix = match old_label.iter().position(|byte| *byte == b':') {
            Some(index) => &old_label[index..],
            None => {
                generated_suffix = format!(":{ordinal}").into_bytes();
                generated_suffix.as_slice()
            }
        };
        let suffix = if suffix.len() > IFNAME_MAX {
            &suffix[suffix.len() - IFNAME_MAX..]
        } else {
            suffix
        };
        let prefix_len = new_name.len().min(IFNAME_MAX - suffix.len());
        let mut label = Vec::with_capacity(prefix_len + suffix.len());
        label.extend_from_slice(&new_name.as_bytes()[..prefix_len]);
        label.extend_from_slice(suffix);
        entry.label = Some(CString::new(label).map_err(|_| SystemError::EINVAL)?);
    }
    Ok(renamed)
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

    // Preserve the established smoltcp -> router projection lock order. Veth
    // ingress reads this projection while holding the smoltcp lock, so
    // publishing it after unlocking would expose a stale-address window.
    let snapshot: Vec<IpCidr> = smol_iface.ip_addrs().to_vec();
    let mut projection = iface.router_common().ip_addrs.write();
    projection.clear();
    projection.extend_from_slice(&snapshot);

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
    let before = iface.router_common().ip_addrs.read().clone();
    let mut after = before.clone();
    let mut metadata = iface.common().address_metadata().lock().clone();

    let outcome = match mutation {
        CommitMutation::Plain(AddressMutation::Add(cidr)) => {
            if find_for_new_or_replace(&after, cidr).is_some() {
                return Err(SystemError::EEXIST);
            }
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

fn insert_address_candidate(addresses: &mut Vec<IpCidr>, cidr: IpCidr) {
    let index = match cidr.address() {
        IpAddress::Ipv4(_) => addresses
            .iter()
            .position(|item| matches!(item.address(), IpAddress::Ipv6(_)))
            .unwrap_or(addresses.len()),
        IpAddress::Ipv6(_) => addresses.len(),
    };
    addresses.insert(index, cidr);
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
