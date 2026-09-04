use alloc::{sync::Arc, vec::Vec};

use smoltcp::iface::Route as SmolRoute;
use system_error::SystemError;

use crate::{
    driver::net::Iface, net::rtnl::RtnlGuard, process::namespace::net_namespace::NetNamespace,
};

use super::{
    fib::{FibEdit, PlannedFibMutation},
    FibEditor, FibTable,
};

pub(super) struct ProjectionPlan {
    replacements: Vec<(Arc<dyn Iface>, Vec<SmolRoute>)>,
}

struct ProjectionUpdate {
    iface: Arc<dyn Iface>,
    cidr: smoltcp::wire::IpCidr,
    after: Option<SmolRoute>,
    had_entry: bool,
}

struct IncrementalProjectionPlan {
    updates: Vec<ProjectionUpdate>,
}

struct PreparedFibEdit<'rtnl, T> {
    _rtnl: &'rtnl RtnlGuard,
    edit: FibEdit,
    projection: IncrementalProjectionPlan,
    outcome: T,
}

impl<T> PreparedFibEdit<'_, T> {
    fn publish(self, netns: &Arc<NetNamespace>) -> T {
        let router = netns.router();
        let mut fib = router.fib_write();
        self.projection.publish();
        fib.apply_edit(self.edit);
        self.outcome
    }
}

impl IncrementalProjectionPlan {
    fn prepare(
        fib: &FibTable,
        edit: FibEdit,
        devices: &[Arc<dyn Iface>],
    ) -> Result<Self, SystemError> {
        let mut updates = Vec::new();
        updates
            .try_reserve_exact(2)
            .map_err(|_| SystemError::ENOMEM)?;
        for key in FibTable::projection_keys(edit).into_iter().flatten() {
            let before = fib.projection_winner(key, None).map(smol_route);
            let after = fib.projection_winner(key, Some(edit)).map(smol_route);
            if same_projection(before, after) {
                continue;
            }
            let iface = devices
                .iter()
                .find(|iface| iface.nic_id() as u32 == key.oif())
                .cloned()
                .ok_or(SystemError::ENODEV)?;
            let cidr = key.cidr();
            let mut had_entry = false;
            iface.smol_iface().lock().routes_mut().update(|routes| {
                had_entry = routes.iter().any(|route| route.cidr == cidr);
            });
            updates.push(ProjectionUpdate {
                iface,
                cidr,
                after,
                had_entry,
            });
        }

        for index in 0..updates.len() {
            if updates[..index]
                .iter()
                .any(|update| update.iface.nic_id() == updates[index].iface.nic_id())
            {
                continue;
            }
            let additions = updates
                .iter()
                .filter(|candidate| {
                    candidate.iface.nic_id() == updates[index].iface.nic_id()
                        && candidate.after.is_some()
                        && !candidate.had_entry
                })
                .count();
            if additions == 0 {
                continue;
            }
            let mut reserve_result = Ok(());
            updates[index]
                .iface
                .smol_iface()
                .lock()
                .routes_mut()
                .update(|routes| {
                    reserve_result = routes
                        .try_reserve(additions)
                        .map_err(|_| SystemError::ENOMEM);
                });
            reserve_result?;
        }
        Ok(Self { updates })
    }

    fn publish(self) {
        for update in self.updates {
            update
                .iface
                .smol_iface()
                .lock()
                .routes_mut()
                .update(|routes| {
                    let existing = routes.iter().position(|route| route.cidr == update.cidr);
                    match (existing, update.after) {
                        (Some(index), Some(route)) => routes[index] = route,
                        (Some(index), None) => {
                            routes.remove(index);
                        }
                        (None, Some(route)) => routes.push(route),
                        (None, None) => {}
                    }
                });
        }
    }
}

fn smol_route(route: super::RouteEntry) -> SmolRoute {
    SmolRoute {
        cidr: super::canonical_cidr(route.destination),
        via_router: route.gateway,
        preferred_until: None,
        expires_at: None,
    }
}

fn same_projection(left: Option<SmolRoute>, right: Option<SmolRoute>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.cidr == right.cidr && left.via_router == right.via_router,
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

/// A route mutation whose allocations and validation have completed.
///
/// RTNL keeps route writers and topology stable between `prepare` and
/// `publish`. Publication only swaps owned projections and the candidate FIB,
/// so callers may safely commit adjacent non-fallible device state in the
/// same control-plane transition.
pub(super) struct PreparedTransaction<'rtnl, T> {
    _rtnl: &'rtnl RtnlGuard,
    candidate: FibTable,
    plan: ProjectionPlan,
    outcome: T,
}

impl<T> PreparedTransaction<'_, T> {
    pub(super) fn publish(self, netns: &Arc<NetNamespace>) -> T {
        self.publish_around(netns, || {}, || {})
    }

    pub(super) fn publish_around(
        self,
        netns: &Arc<NetNamespace>,
        before_routes: impl FnOnce(),
        after_routes: impl FnOnce(),
    ) -> T {
        let router = netns.router();
        let mut current = router.fib_write();
        before_routes();
        self.plan.publish();
        *current = self.candidate;
        after_routes();
        self.outcome
    }
}

impl ProjectionPlan {
    pub(super) fn prepare(
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

    pub(super) fn publish(self) {
        for (iface, mut projection) in self.replacements {
            iface.smol_iface().lock().routes_mut().update(|routes| {
                core::mem::swap(routes, &mut projection);
            });
        }
    }
}

/// Apply one ordinary route edit without cloning or rebuilding the whole FIB.
///
/// RTNL stabilizes writers while capacity and the exact smoltcp projection
/// delta are prepared. Capacity-only reservations are not user-visible; the
/// eventual publication contains no fallible operation.
pub(super) fn transact_single<T>(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    plan: impl FnOnce(&FibTable) -> Result<PlannedFibMutation<T>, SystemError>,
) -> Result<T, SystemError> {
    let device_list = netns.device_list();
    let mut devices = Vec::new();
    devices
        .try_reserve_exact(device_list.len())
        .map_err(|_| SystemError::ENOMEM)?;
    devices.extend(device_list.values().cloned());
    drop(device_list);

    let router = netns.router();
    let mut fib = router.fib_write();
    let planned = plan(&fib)?;
    fib.reserve_edit(planned.edit)?;
    let projection = match IncrementalProjectionPlan::prepare(&fib, planned.edit, &devices) {
        Ok(projection) => projection,
        Err(error) => {
            fib.cancel_edit_reservation(planned.edit);
            return Err(error);
        }
    };
    drop(fib);
    Ok(PreparedFibEdit {
        _rtnl: rtnl,
        edit: planned.edit,
        projection,
        outcome: planned.outcome,
    }
    .publish(netns))
}

pub(super) fn transact_with_devices<T>(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    devices: &[Arc<dyn Iface>],
    mutate: impl FnOnce(&mut FibEditor) -> Result<T, SystemError>,
) -> Result<T, SystemError> {
    Ok(prepare_with_devices(rtnl, netns, devices, mutate)?.publish(netns))
}

pub(super) fn prepare_with_devices<'rtnl, T>(
    rtnl: &'rtnl RtnlGuard,
    netns: &Arc<NetNamespace>,
    devices: &[Arc<dyn Iface>],
    mutate: impl FnOnce(&mut FibEditor) -> Result<T, SystemError>,
) -> Result<PreparedTransaction<'rtnl, T>, SystemError> {
    let router = netns.router();
    // RTNL keeps topology and writers stable. Candidate construction and
    // projection preparation stay outside the FIB write-side critical path.
    // Do not carry the FIB lock into mutation callbacks or interface locking:
    // RTNL already stabilizes writers, while an owned snapshot keeps the
    // cross-subsystem lock order acyclic.
    let mut candidate = router.fib.read().try_clone()?;
    let mut editor = FibEditor::new(&mut candidate);
    let outcome = mutate(&mut editor)?;
    let affected_oifs = editor.finish()?;
    let before = router.fib.read();
    let plan = ProjectionPlan::prepare(&before, &candidate, &affected_oifs, devices)?;
    drop(before);

    Ok(PreparedTransaction {
        _rtnl: rtnl,
        candidate,
        plan,
        outcome,
    })
}

pub(super) fn projection_for_iface(
    fib: &FibTable,
    ifindex: u32,
) -> Result<Vec<SmolRoute>, SystemError> {
    fib.projection_for_iface(ifindex)
}

fn projections_equal(left: &[SmolRoute], right: &[SmolRoute]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.cidr == right.cidr && left.via_router == right.via_router)
}
