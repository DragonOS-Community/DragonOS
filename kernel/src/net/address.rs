//! Protocol-independent interface address mutation core.
//!
//! Runtime callers must hold RTNL for the whole operation.  The smoltcp
//! address list, its connected-route projection, and the router compatibility
//! projection are committed under one smoltcp interface lock.

use alloc::{sync::Arc, vec::Vec};

use smoltcp::{
    iface::Route,
    wire::{IpAddress, IpCidr},
};
use system_error::SystemError;

use crate::{driver::net::Iface, net::rtnl::RtnlGuard};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AddressMutation {
    Add(IpCidr),
    Delete(IpCidr),
    Replace(IpCidr),
    ExchangeOwned { old: IpCidr, new: IpCidr },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AddressMutationOutcome {
    Added(IpCidr),
    Deleted(IpCidr),
    Replaced(IpCidr),
    Exchanged { old: IpCidr, new: IpCidr },
}

/// Mutates an address on an interface that is already visible in a netns.
pub(in crate::net) fn mutate_address(
    _rtnl: &RtnlGuard,
    iface: &Arc<dyn Iface>,
    mutation: AddressMutation,
) -> Result<AddressMutationOutcome, SystemError> {
    commit(iface, mutation)
}

/// Initializes an address before an interface is published in a netns.
///
/// This narrow construction-time entry point exists so drivers do not grow a
/// second, weaker address mutation path. Published interfaces must use
/// [`mutate_address`] under RTNL.
pub(crate) fn initialize_address(iface: &Arc<dyn Iface>, cidr: IpCidr) -> Result<(), SystemError> {
    if iface.net_namespace().is_some() {
        return Err(SystemError::EBUSY);
    }
    commit(iface, AddressMutation::Add(cidr)).map(|_| ())
}

fn commit(
    iface: &Arc<dyn Iface>,
    mutation: AddressMutation,
) -> Result<AddressMutationOutcome, SystemError> {
    validate_mutation(mutation)?;

    let mut smol_iface = iface.smol_iface().lock();
    let outcome = match mutation {
        AddressMutation::Add(cidr) => add(&mut smol_iface, cidr)?,
        AddressMutation::Delete(cidr) => delete(&mut smol_iface, cidr)?,
        AddressMutation::Replace(cidr) => replace(&mut smol_iface, cidr)?,
        AddressMutation::ExchangeOwned { old, new } => exchange_owned(&mut smol_iface, old, new)?,
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

fn validate_mutation(mutation: AddressMutation) -> Result<(), SystemError> {
    match mutation {
        AddressMutation::Add(cidr)
        | AddressMutation::Delete(cidr)
        | AddressMutation::Replace(cidr) => validate_cidr(cidr),
        AddressMutation::ExchangeOwned { old, new } => {
            validate_cidr(old)?;
            validate_cidr(new)?;
            if same_family(old, new) {
                Ok(())
            } else {
                Err(SystemError::EINVAL)
            }
        }
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

    if !insert_connected_route(iface, cidr) {
        iface.update_ip_addrs(|addresses| {
            if let Some(index) = addresses.iter().position(|item| *item == cidr) {
                addresses.remove(index);
            }
        });
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
    remove_connected_route(iface, effective);

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

    if !ensure_connected_route(iface, effective) {
        return Err(SystemError::ENOSPC);
    }
    Ok(AddressMutationOutcome::Replaced(effective))
}

fn exchange_owned(
    iface: &mut smoltcp::iface::Interface,
    old: IpCidr,
    new: IpCidr,
) -> Result<AddressMutationOutcome, SystemError> {
    let old_index = find_for_delete(iface.ip_addrs(), old).ok_or(SystemError::EADDRNOTAVAIL)?;
    if old == new {
        if !ensure_connected_route(iface, old) {
            return Err(SystemError::ENOSPC);
        }
        return Ok(AddressMutationOutcome::Replaced(old));
    }
    if iface
        .ip_addrs()
        .iter()
        .enumerate()
        .any(|(index, configured)| {
            index != old_index && same_new_or_replace_identity(*configured, new)
        })
    {
        return Err(SystemError::EEXIST);
    }

    // Prepare the route transition first. Replacing the address at a known
    // index cannot fail, so an ENOSPC return still leaves the old state intact.
    let mut route_ready = false;
    iface.routes_mut().update(|routes| {
        let old_index = routes.iter().position(|route| is_connected(route, old));

        if let Some(index) = old_index {
            routes[index].cidr = new;
            route_ready = true;
        } else {
            let index = routes
                .iter()
                .position(|route| is_connected(route, new))
                .unwrap_or(routes.len());
            route_ready = routes.insert(index, connected_route(new)).is_ok();
        }
    });
    if !route_ready {
        return Err(SystemError::ENOSPC);
    }

    iface.update_ip_addrs(|addresses| addresses[old_index] = new);
    Ok(AddressMutationOutcome::Exchanged { old, new })
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

fn same_family(left: IpCidr, right: IpCidr) -> bool {
    matches!(
        (left.address(), right.address()),
        (IpAddress::Ipv4(_), IpAddress::Ipv4(_)) | (IpAddress::Ipv6(_), IpAddress::Ipv6(_))
    )
}

fn ensure_connected_route(iface: &mut smoltcp::iface::Interface, cidr: IpCidr) -> bool {
    let mut ready = false;
    iface.routes_mut().update(|routes| {
        if routes.iter().any(|route| is_connected(route, cidr)) {
            ready = true;
        } else {
            ready = routes.push(connected_route(cidr)).is_ok();
        }
    });
    ready
}

fn insert_connected_route(iface: &mut smoltcp::iface::Interface, cidr: IpCidr) -> bool {
    let mut inserted = false;
    iface.routes_mut().update(|routes| {
        // Put the derived projection before an equal explicit route. Without
        // owner metadata this stable ordering lets Delete remove exactly the
        // address-owned slot while preserving userspace's route object.
        let index = routes
            .iter()
            .position(|route| is_connected(route, cidr))
            .unwrap_or(routes.len());
        inserted = routes.insert(index, connected_route(cidr)).is_ok();
    });
    inserted
}

fn remove_connected_route(iface: &mut smoltcp::iface::Interface, cidr: IpCidr) {
    iface.routes_mut().update(|routes| {
        if let Some(index) = routes.iter().position(|route| is_connected(route, cidr)) {
            routes.remove(index);
        }
    });
}

fn connected_route(cidr: IpCidr) -> Route {
    Route {
        cidr,
        via_router: None,
        preferred_until: None,
        expires_at: None,
    }
}

#[inline]
fn is_connected(route: &Route, cidr: IpCidr) -> bool {
    route.cidr == cidr && route.via_router.is_none()
}
