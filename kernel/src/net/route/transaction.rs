use alloc::{sync::Arc, vec::Vec};

use smoltcp::{iface::Route as SmolRoute, wire::IpCidr};
use system_error::SystemError;

use crate::{
    driver::net::Iface, net::rtnl::RtnlGuard, process::namespace::net_namespace::NetNamespace,
};

use super::{
    canonical_cidr, FibEditor, FibTable, RTN_LOCAL, RTN_UNICAST, RT_TABLE_LOCAL, RT_TABLE_MAIN,
};

struct ProjectionPlan {
    replacements: Vec<(Arc<dyn Iface>, Vec<SmolRoute>)>,
}

impl ProjectionPlan {
    fn prepare(
        before: &FibTable,
        after: &FibTable,
        affected_oifs: &[u32],
        devices: &[Arc<dyn Iface>],
    ) -> Result<Self, SystemError> {
        let mut replacements = Vec::new();
        for iface in devices
            .iter()
            .filter(|iface| affected_oifs.contains(&(iface.nic_id() as u32)))
        {
            let ifindex = iface.nic_id() as u32;
            let projection = projection_for_iface(after, ifindex)?;
            if projections_equal(&projection, &projection_for_iface(before, ifindex)?) {
                continue;
            }
            replacements
                .try_reserve(1)
                .map_err(|_| SystemError::ENOMEM)?;
            replacements.push((iface.clone(), projection));
        }
        replacements.sort_unstable_by_key(|(iface, _)| iface.nic_id());
        Ok(Self { replacements })
    }

    fn publish(self) {
        for (iface, mut projection) in self.replacements {
            iface.smol_iface().lock().routes_mut().update(|routes| {
                core::mem::swap(routes, &mut projection);
            });
        }
    }
}

pub(super) fn transact<T>(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    mutate: impl FnOnce(&mut FibEditor) -> Result<T, SystemError>,
) -> Result<T, SystemError> {
    let device_list = netns.device_list();
    let mut devices = Vec::new();
    devices
        .try_reserve_exact(device_list.len())
        .map_err(|_| SystemError::ENOMEM)?;
    devices.extend(device_list.values().cloned());
    drop(device_list);
    transact_with_devices(rtnl, netns, &devices, mutate)
}

pub(super) fn transact_with_devices<T>(
    _rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    devices: &[Arc<dyn Iface>],
    mutate: impl FnOnce(&mut FibEditor) -> Result<T, SystemError>,
) -> Result<T, SystemError> {
    let router = netns.router();
    // RTNL keeps topology and writers stable. Candidate construction and
    // projection preparation stay outside the FIB write-side critical path.
    // Do not carry the FIB lock into mutation callbacks or interface locking:
    // RTNL already stabilizes writers, while an owned snapshot keeps the
    // cross-subsystem lock order acyclic.
    let before = router.fib.read().try_clone()?;
    let mut candidate = before.try_clone()?;
    let mut editor = FibEditor::new(&mut candidate);
    let outcome = mutate(&mut editor)?;
    let affected_oifs = editor.finish();
    let plan = ProjectionPlan::prepare(&before, &candidate, &affected_oifs, devices)?;

    let mut current = router.fib.write();
    debug_assert_eq!(*current, before);
    plan.publish();
    *current = candidate;
    Ok(outcome)
}

pub(super) fn projection_for_iface(
    fib: &FibTable,
    ifindex: u32,
) -> Result<Vec<SmolRoute>, SystemError> {
    let is_projectable = |route: &super::RouteEntry| {
        route.oif == ifindex
            && route.source.is_none()
            && (route.table == RT_TABLE_MAIN && route.kind == RTN_UNICAST
                || route.table == RT_TABLE_LOCAL && route.kind == RTN_LOCAL)
    };
    let mut candidates = Vec::new();
    for (index, route) in fib.entries().iter().copied().enumerate() {
        if is_projectable(&route) {
            candidates.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
            candidates.push((index, route));
        }
    }
    candidates.sort_unstable_by_key(|(index, route)| {
        (
            canonical_cidr(route.destination),
            u8::from(route.table != RT_TABLE_LOCAL),
            route.priority,
            *index,
        )
    });

    let mut projection = Vec::new();
    let mut last_cidr: Option<IpCidr> = None;
    for (_, route) in candidates {
        let cidr = canonical_cidr(route.destination);
        if last_cidr == Some(cidr) {
            continue;
        }
        projection.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        projection.push(SmolRoute {
            cidr,
            via_router: route.gateway,
            preferred_until: None,
            expires_at: None,
        });
        last_cidr = Some(cidr);
    }
    Ok(projection)
}

fn projections_equal(left: &[SmolRoute], right: &[SmolRoute]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.cidr == right.cidr && left.via_router == right.via_router)
}
