//! ABI-independent network-link mutation core.
//!
//! Rtnetlink and legacy socket ioctls translate their wire formats into these
//! types. RTNL remains the outer writer lock; all fallible preparation is
//! completed before authoritative interface state is published.

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::fmt::Write;

use system_error::SystemError;

use crate::{
    arch::MMArch,
    driver::{
        base::device_rename::PreparedDeviceSysfsRename,
        net::{
            bridge::BridgeIface,
            napi::{napi_pause_and_wait, napi_resume, napi_schedule},
            sysfs::prepare_netdev_sysfs_rename,
            types::InterfaceFlags,
            veth::VethInterface,
            Iface, NetDeivceState, Operstate, PreparedConfiguredFlags,
        },
    },
    mm::MemoryManagementArch,
    process::namespace::net_namespace::NetNamespace,
};

use super::{
    address::{PreparedAddressLabelRename, PreparedMtuAddressChange, RemovedAddress},
    neighbor::{self, NeighborEntry, PreparedConfiguredNeighborPurge},
    route::{self, FibTable, PreparedLinkStateChange, RouteNotifications},
    rtnl::RtnlGuard,
};

const IFNAMSIZ: usize = 16;

pub(crate) mod topology;

const USER_SETTABLE_FLAGS: InterfaceFlags = InterfaceFlags::from_bits_truncate(
    InterfaceFlags::UP.bits()
        | InterfaceFlags::DEBUG.bits()
        | InterfaceFlags::NOTRAILERS.bits()
        | InterfaceFlags::NOARP.bits()
        | InterfaceFlags::PROMISC.bits()
        | InterfaceFlags::ALLMULTI.bits()
        | InterfaceFlags::DYNAMIC.bits()
        | InterfaceFlags::MULTICAST.bits()
        | InterfaceFlags::PORTSEL.bits()
        | InterfaceFlags::AUTOMEDIA.bits(),
);

pub(crate) enum LinkTarget<'a> {
    Index(u32),
    Name(&'a str),
}

#[derive(Default)]
pub(crate) struct LinkUpdate {
    pub new_name: Option<String>,
    pub mtu: Option<LinkMtuUpdate>,
    pub tx_queue_len: Option<u32>,
    pub flags: Option<LinkFlagsUpdate>,
}

#[derive(Clone, Copy)]
pub(crate) enum LinkMtuUpdate {
    Rtnetlink(u32),
    Ioctl(i32),
    Auto(usize),
}

pub(crate) enum LinkFlagsUpdate {
    Replace(InterfaceFlags),
    Masked {
        requested: InterfaceFlags,
        change: InterfaceFlags,
    },
}

bitflags! {
    pub(crate) struct LinkChanges: u8 {
        const NAME = 1 << 0;
        const MTU = 1 << 1;
        const FLAGS = 1 << 2;
        const TX_QUEUE_LEN = 1 << 3;
    }
}

pub(crate) struct LinkMutationCommit {
    pub iface: Arc<dyn Iface>,
    pub changes: LinkChanges,
    pub renamed_ipv4: Vec<smoltcp::wire::IpCidr>,
    pub removed_addresses: Vec<RemovedAddress>,
    pub route_changes: Option<RouteNotifications>,
    pub removed_neighbors: Vec<NeighborEntry>,
    pub rename_old_devpath: Option<String>,
    /// At most one master bridge can be affected by a veth MTU mutation.
    pub related: Option<Box<LinkMutationCommit>>,
}

/// A bridge-only MTU update whose fallible address/FIB preparation has
/// completed. Its commit cannot fail because it contains no sysfs rename.
pub(crate) struct PreparedAutoMtu<'rtnl>(PreparedLinkMutation<'rtnl>);

/// Opaque view of a preceding prepared link mutation's FIB candidate.
#[derive(Clone, Copy)]
pub(crate) struct StagedLinkFib<'a>(&'a FibTable);

impl StagedLinkFib<'_> {
    pub(in crate::net) fn candidate(&self) -> &FibTable {
        self.0
    }
}

impl PreparedAutoMtu<'_> {
    pub(crate) fn staged_fib(&self) -> Option<StagedLinkFib<'_>> {
        self.0.staged_fib()
    }

    pub(crate) fn commit(self) -> LinkMutationCommit {
        self.0
            .commit()
            .expect("prepared automatic MTU change contains no fallible rename")
    }
}

struct PreparedLinkRename {
    name: String,
    labels: Option<PreparedAddressLabelRename>,
    sysfs: PreparedDeviceSysfsRename,
}

struct PreparedLinkMutation<'rtnl> {
    _rtnl: &'rtnl RtnlGuard,
    iface: Arc<dyn Iface>,
    netns: Arc<NetNamespace>,
    mtu: Option<usize>,
    tx_queue_len: Option<u32>,
    rename: Option<PreparedLinkRename>,
    flags: Option<PreparedConfiguredFlags>,
    routes: Option<PreparedLinkStateChange<'rtnl>>,
    address_change: Option<PreparedMtuAddressChange>,
    neighbors: Option<PreparedConfiguredNeighborPurge<'rtnl>>,
    changes: LinkChanges,
    was_up: bool,
    is_up: bool,
    noarp_changed: bool,
    explicit_mtu: bool,
}

pub(crate) fn prepare_auto_mtu<'rtnl>(
    rtnl: &'rtnl RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: Arc<dyn Iface>,
    mtu: usize,
    staged_before: Option<StagedLinkFib<'_>>,
) -> Result<PreparedAutoMtu<'rtnl>, SystemError> {
    let plan = PreparedLinkMutation::prepare(
        rtnl,
        netns.clone(),
        iface,
        LinkUpdate {
            mtu: Some(LinkMtuUpdate::Auto(mtu)),
            ..Default::default()
        },
        staged_before.map(|staged| staged.0),
    )?;
    Ok(PreparedAutoMtu(plan))
}

pub(crate) fn mutate_link(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    target: LinkTarget<'_>,
    update: LinkUpdate,
) -> Result<LinkMutationCommit, SystemError> {
    let iface = resolve_iface(netns, target)?;
    let prepared = PreparedLinkMutation::prepare(rtnl, netns.clone(), iface, update, None)?;
    let bridge_update = if let Some(mtu) = prepared.mtu {
        if let Some(veth) = prepared.iface.as_any_ref().downcast_ref::<VethInterface>() {
            if let Some(bridge) = veth.bridge_master() {
                bridge.prepare_port_mtu_change(rtnl, netns, veth, mtu, prepared.staged_fib())?
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };
    // Reserve storage for the one possible master-bridge notification before
    // publishing either device. Allocation must not follow the first commit.
    let related_slot = if bridge_update.is_some() {
        Some(Box::<LinkMutationCommit>::try_new_uninit().map_err(|_| SystemError::ENOMEM)?)
    } else {
        None
    };
    let mut committed = prepared.commit()?;
    if let (Some(bridge_update), Some(mut slot)) = (bridge_update, related_slot) {
        slot.as_mut().write(bridge_update.commit());
        // SAFETY: the prepared bridge commit was written into the boxed slot.
        committed.related = Some(unsafe { slot.assume_init() });
    }
    Ok(committed)
}

impl<'rtnl> PreparedLinkMutation<'rtnl> {
    fn staged_fib(&self) -> Option<StagedLinkFib<'_>> {
        self.address_change
            .as_ref()
            .map(|change| StagedLinkFib(change.candidate()))
            .or_else(|| {
                self.routes
                    .as_ref()
                    .map(|routes| StagedLinkFib(routes.candidate()))
            })
    }

    fn prepare(
        rtnl: &'rtnl RtnlGuard,
        netns: Arc<NetNamespace>,
        iface: Arc<dyn Iface>,
        update: LinkUpdate,
        staged_before: Option<&FibTable>,
    ) -> Result<Self, SystemError> {
        // Keep Linux do_setlink()'s externally observable validation order.
        let explicit_mtu = matches!(
            update.mtu,
            Some(LinkMtuUpdate::Rtnetlink(_) | LinkMtuUpdate::Ioctl(_))
        );
        let mtu = prepare_mtu(&iface, update.mtu)?;
        let tx_queue_len = update
            .tx_queue_len
            .filter(|len| *len != iface.common().tx_queue_len());
        let rename = prepare_rename(rtnl, &netns, &iface, update.new_name)?;

        let (flags, was_up, is_up, noarp_changed) = prepare_flags(&iface, update.flags)?;
        let link_down = was_up && !is_up;
        let address_change = mtu
            .map(|mtu| {
                let name = rename.as_ref().map(|rename| rename.name.as_str());
                super::address::prepare_mtu_address_change(
                    &netns,
                    &iface,
                    mtu,
                    name,
                    was_up,
                    is_up,
                    staged_before,
                )
            })
            .transpose()?
            .flatten();
        let mut rename = rename;
        if address_change.is_some() {
            if let Some(rename) = rename.as_mut() {
                rename.labels = None;
            }
        }
        let routes = if address_change.is_none() && was_up != is_up {
            assert!(
                staged_before.is_none(),
                "staged MTU update cannot change link state"
            );
            Some(route::prepare_link_state_change(
                rtnl, &netns, &iface, is_up,
            )?)
        } else {
            None
        };
        let neighbors = if link_down || (noarp_changed && is_up) {
            Some(neighbor::prepare_configured_iface_purge(
                rtnl,
                &netns,
                iface.nic_id() as u32,
            )?)
        } else {
            None
        };

        let mut changes = LinkChanges::empty();
        if mtu.is_some() {
            changes.insert(LinkChanges::MTU);
        }
        if tx_queue_len.is_some() {
            changes.insert(LinkChanges::TX_QUEUE_LEN);
        }
        if rename.is_some() {
            changes.insert(LinkChanges::NAME);
        }
        if flags.is_some_and(|prepared| prepared.old_flags() != prepared.new_flags()) {
            changes.insert(LinkChanges::FLAGS);
        }

        Ok(Self {
            _rtnl: rtnl,
            iface,
            netns,
            mtu,
            tx_queue_len,
            rename,
            flags,
            routes,
            address_change,
            neighbors,
            changes,
            was_up,
            is_up,
            noarp_changed,
            explicit_mtu,
        })
    }

    fn commit(self) -> Result<LinkMutationCommit, SystemError> {
        let Self {
            _rtnl: _,
            iface,
            netns,
            mtu,
            tx_queue_len,
            rename,
            flags,
            routes,
            address_change,
            neighbors,
            changes,
            was_up,
            is_up,
            noarp_changed,
            explicit_mtu,
        } = self;

        // The kernfs transaction performs its final conflict/generation check
        // before any logical link state is changed.
        let (rename, rename_old_devpath) = if let Some(rename) = rename {
            let old_devpath = rename.sysfs.commit()?;
            (Some((rename.name, rename.labels)), old_devpath)
        } else {
            (None, None)
        };

        if let Some(tx_queue_len) = tx_queue_len {
            iface.common().set_tx_queue_len(tx_queue_len);
        }

        if let Some(mtu) = mtu.filter(|_| address_change.is_none()) {
            let stack_mtu = iface.stack_mtu(mtu);
            iface
                .smol_iface()
                .lock()
                .set_ip_mtu(stack_mtu)
                .expect("validated interface MTU must be representable by smoltcp");
            iface.common().set_mtu(mtu);
        }

        let mut rename = rename;
        let mut renamed_ipv4 = Vec::new();
        if address_change.is_none() {
            if let Some((name, labels)) = rename.take() {
                renamed_ipv4 = labels
                    .expect("rename without an address transaction owns its label plan")
                    .publish(&iface, name);
            }
        }

        if noarp_changed {
            iface
                .smol_iface()
                .lock()
                .set_neighbor_discovery_enabled(!is_noarp(flags));
        }

        let link_down = was_up && !is_up;
        if neighbors.is_some() && !link_down {
            iface.smol_iface().lock().flush_neighbor_cache();
        }

        if link_down {
            iface.common().close_tx_and_wait();
            iface.begin_admin_down();
            if let Some(napi) = iface.napi_struct() {
                napi_pause_and_wait(&napi);
            }
            // This can wait for device-private ingress. It deliberately runs
            // before the FIB writer so a classifier holding a FIB reader can
            // finish instead of deadlocking the control-plane transaction.
            iface.quiesce_admin_down();
        }

        let mut removed_addresses = Vec::new();
        let route_changes = if let Some(address_change) = address_change {
            let renamed_to = rename.take().map(|(name, labels)| {
                debug_assert!(labels.is_none());
                name
            });
            let (removed, renamed, route_changes) = address_change.publish(
                &netns,
                &iface,
                renamed_to,
                || {
                    let mtu = mtu.expect("address teardown requires an MTU update");
                    let stack_mtu = iface.stack_mtu(mtu);
                    iface
                        .smol_iface()
                        .lock()
                        .set_ip_mtu(stack_mtu)
                        .expect("validated interface MTU must be representable by smoltcp");
                    iface.common().set_mtu(mtu);
                },
                is_up,
                || {
                    if was_up != is_up {
                        publish_link_flags_and_state(&iface, flags, is_up);
                    } else if let Some(flags) = flags {
                        iface.common().publish_configured_flags(flags);
                    }
                    if link_down {
                        iface.smol_iface().lock().flush_neighbor_cache();
                    }
                },
            );
            removed_addresses = removed;
            renamed_ipv4 = renamed;
            Some(route_changes)
        } else if let Some(routes) = routes {
            Some(routes.publish(&netns, is_up, || {
                publish_link_flags_and_state(&iface, flags, is_up);
                if link_down {
                    // The route transaction holds the FIB writer here. Pollers
                    // which observed the old UP generation have drained before
                    // this final device/L2 invalidation, and later pollers see
                    // DOWN, so no learned mapping can cross the transition.
                    iface.smol_iface().lock().flush_neighbor_cache();
                }
            }))
        } else {
            if let Some(flags) = flags {
                iface.common().publish_configured_flags(flags);
            }
            None
        };

        let removed_neighbors = neighbors.map(|plan| plan.publish()).unwrap_or_default();

        // A NOARP transition changes whether an unresolved destination can be
        // sent immediately. Wake the owner so packets waiting on the previous
        // discovery policy are re-evaluated without waiting for an old retry
        // deadline.
        let link_state_changed = was_up != is_up;
        if link_state_changed && is_up {
            if iface.napi_struct().is_none() {
                netns.wakeup_poll_thread();
            }
        } else if noarp_changed {
            if let Some(napi) = iface.napi_struct() {
                napi_schedule(napi);
            } else {
                netns.wakeup_poll_thread();
            }
        }
        if noarp_changed || link_state_changed {
            netns.notify_deadline_changed();
        }

        if explicit_mtu && mtu.is_some() {
            if let Some(bridge) = iface.as_any_ref().downcast_ref::<BridgeIface>() {
                bridge.bridge_driver().mark_mtu_set_by_user();
            }
        }

        Ok(LinkMutationCommit {
            iface,
            changes,
            renamed_ipv4,
            removed_addresses,
            route_changes,
            removed_neighbors,
            rename_old_devpath,
            related: None,
        })
    }
}

fn resolve_iface(
    netns: &Arc<NetNamespace>,
    target: LinkTarget<'_>,
) -> Result<Arc<dyn Iface>, SystemError> {
    let devices = netns.device_list();
    let iface = match target {
        LinkTarget::Index(index) => devices.get(&(index as usize)).cloned(),
        LinkTarget::Name(name) => devices
            .values()
            .find(|iface| iface.common().with_iface_name(|current| current == name))
            .cloned(),
    }
    .ok_or(SystemError::ENODEV)?;
    drop(devices);

    if !iface
        .net_namespace()
        .is_some_and(|owner| Arc::ptr_eq(&owner, netns))
        || !iface
            .net_state()
            .contains(NetDeivceState::__LINK_STATE_PRESENT)
    {
        return Err(SystemError::ENODEV);
    }
    Ok(iface)
}

fn prepare_mtu(
    iface: &Arc<dyn Iface>,
    requested: Option<LinkMtuUpdate>,
) -> Result<Option<usize>, SystemError> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    let requested = match requested {
        LinkMtuUpdate::Rtnetlink(value) => usize::try_from(value),
        LinkMtuUpdate::Ioctl(value) => usize::try_from(value),
        LinkMtuUpdate::Auto(value) => Ok(value),
    }
    .map_err(|_| SystemError::EINVAL)?;
    let bounds = iface.mtu_bounds();
    if requested < bounds.min || requested > bounds.max {
        return Err(SystemError::EINVAL);
    }
    Ok((requested != iface.mtu()).then_some(requested))
}

fn prepare_rename(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    requested: Option<String>,
) -> Result<Option<PreparedLinkRename>, SystemError> {
    let Some(template) = requested else {
        return Ok(None);
    };
    let name = allocate_link_name(netns, iface, &template)?;
    if iface
        .common()
        .with_iface_name(|current| current == name.as_str())
    {
        return Ok(None);
    }

    let labels = PreparedAddressLabelRename::prepare(rtnl, iface, &name)?;
    let sysfs = prepare_netdev_sysfs_rename(iface, try_copy_string(&name)?)?;
    Ok(Some(PreparedLinkRename {
        name,
        labels: Some(labels),
        sysfs,
    }))
}

fn allocate_link_name(
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    template: &str,
) -> Result<String, SystemError> {
    validate_name_bytes(template)?;
    let percent = template.as_bytes().iter().position(|byte| *byte == b'%');
    let Some(percent) = percent else {
        if name_is_used(netns, iface, template) {
            return Err(SystemError::EEXIST);
        }
        return try_copy_string(template);
    };
    if template.as_bytes().get(percent + 1) != Some(&b'd')
        || template.as_bytes()[percent + 2..].contains(&b'%')
    {
        return Err(SystemError::EINVAL);
    }

    let prefix = &template[..percent];
    let suffix = &template[percent + 2..];
    let limit = 8 * <MMArch as MemoryManagementArch>::PAGE_SIZE;
    let mut used = Vec::new();
    let words = limit.div_ceil(u64::BITS as usize);
    used.try_reserve_exact(words)
        .map_err(|_| SystemError::ENOMEM)?;
    used.resize(words, 0u64);
    for other in netns.device_list().values() {
        other.common().with_iface_name(|current| {
            if let Some(ordinal) = template_ordinal(current, prefix, suffix, limit) {
                used[ordinal / u64::BITS as usize] |= 1u64 << (ordinal % u64::BITS as usize);
            }
        });
    }
    for ordinal in 0..limit {
        let digits = decimal_digits(ordinal);
        if prefix.len() + digits + suffix.len() >= IFNAMSIZ {
            continue;
        }
        if used[ordinal / u64::BITS as usize] & (1u64 << (ordinal % u64::BITS as usize)) != 0 {
            continue;
        }
        let mut candidate = String::new();
        candidate
            .try_reserve_exact(IFNAMSIZ - 1)
            .map_err(|_| SystemError::ENOMEM)?;
        candidate.push_str(prefix);
        write!(&mut candidate, "{ordinal}").map_err(|_| SystemError::ENOMEM)?;
        candidate.push_str(suffix);
        return Ok(candidate);
    }
    Err(SystemError::ENFILE)
}

fn template_ordinal(name: &str, prefix: &str, suffix: &str, limit: usize) -> Option<usize> {
    let middle = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    if middle.is_empty()
        || (middle.len() > 1 && middle.starts_with('0'))
        || !middle.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let ordinal = middle.parse::<usize>().ok()?;
    (ordinal < limit).then_some(ordinal)
}

fn decimal_digits(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn validate_name_bytes(name: &str) -> Result<(), SystemError> {
    if name.is_empty()
        || name.len() >= IFNAMSIZ
        || name == "."
        || name == ".."
        || name
            .bytes()
            .any(|byte| byte == b'/' || byte == b':' || byte.is_ascii_whitespace())
    {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

fn name_is_used(netns: &Arc<NetNamespace>, iface: &Arc<dyn Iface>, name: &str) -> bool {
    netns.device_list().values().any(|other| {
        !Arc::ptr_eq(other, iface) && other.common().with_iface_name(|current| current == name)
    })
}

fn prepare_flags(
    iface: &Arc<dyn Iface>,
    update: Option<LinkFlagsUpdate>,
) -> Result<(Option<PreparedConfiguredFlags>, bool, bool, bool), SystemError> {
    let Some(update) = update else {
        let up = iface
            .common()
            .configured_flags()
            .contains(InterfaceFlags::UP);
        return Ok((None, up, up, false));
    };
    let (requested, change) = match update {
        LinkFlagsUpdate::Replace(requested) => (requested, USER_SETTABLE_FLAGS),
        LinkFlagsUpdate::Masked { requested, change } => {
            let change = if change.is_empty() {
                USER_SETTABLE_FLAGS
            } else {
                change & USER_SETTABLE_FLAGS
            };
            (requested, change)
        }
    };
    let prepared = iface.common().prepare_configured_flags(requested, change)?;
    let old = prepared.old_flags();
    let new = prepared.new_flags();
    Ok((
        Some(prepared),
        old.contains(InterfaceFlags::UP),
        new.contains(InterfaceFlags::UP),
        old.contains(InterfaceFlags::NOARP) != new.contains(InterfaceFlags::NOARP),
    ))
}

fn is_noarp(flags: Option<PreparedConfiguredFlags>) -> bool {
    flags.is_some_and(|prepared| prepared.new_flags().contains(InterfaceFlags::NOARP))
}

fn publish_link_flags_and_state(
    iface: &Arc<dyn Iface>,
    flags: Option<PreparedConfiguredFlags>,
    is_up: bool,
) {
    if is_up {
        iface.publish_admin_state(true);
        if let Some(flags) = flags {
            iface.common().publish_configured_flags(flags);
        }
        // Administrative UP does not imply carrier. In particular, a veth
        // whose peer is still DOWN must not report an operational UP link.
        iface.set_operstate(
            if iface
                .net_state()
                .contains(NetDeivceState::__LINK_STATE_NOCARRIER)
            {
                Operstate::IF_OPER_DOWN
            } else {
                Operstate::IF_OPER_UP
            },
        );
        iface.set_net_state(NetDeivceState::__LINK_STATE_START);
        iface.common().open_tx();
    } else {
        iface.publish_admin_state(false);
        if let Some(flags) = flags {
            iface.common().publish_configured_flags(flags);
        }
        iface.clear_net_state(NetDeivceState::__LINK_STATE_START);
        iface.set_operstate(Operstate::IF_OPER_DOWN);
    }
    // Pause protects the transition, not the entire DOWN lifetime. A DOWN
    // interface still owns local routes and must poll namespace-local input.
    // poll_scope() excludes physical ingress until the link is UP again.
    if let Some(napi) = iface.napi_struct() {
        napi_resume(napi);
    }
}

fn try_copy_string(source: &str) -> Result<String, SystemError> {
    let mut result = String::new();
    result
        .try_reserve_exact(source.len())
        .map_err(|_| SystemError::ENOMEM)?;
    result.push_str(source);
    Ok(result)
}
