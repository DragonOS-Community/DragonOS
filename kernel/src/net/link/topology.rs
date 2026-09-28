//! Dynamic bridge/veth topology operations serialized by RTNL.
//!
//! The rtnetlink layer validates wire input; this module owns the device
//! lifecycle and keeps driver-specific bookkeeping out of the ABI parser.

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    driver::{
        base::kobject::KObject,
        net::{
            bridge::BridgeIface,
            napi::{napi_enable, napi_resume},
            prepare_unregister_netdevices_from_locked, prepare_unregister_netdevices_locked,
            register_netdevice_locked,
            sysfs::{
                netdev_emit_uevent, netdev_register_kobject, netdev_unregister_kobject,
                prepare_netdev_sysfs_move,
            },
            veth::VethInterface,
            Iface, NetDeivceState,
        },
    },
    libs::casting::DowncastArc,
    net::rtnl::RtnlGuard,
    process::namespace::net_namespace::NetNamespace,
};

use super::validate_name_bytes;
use crate::net::address::detach_for_netns_move;

fn as_veth(iface: Arc<dyn Iface>) -> Option<Arc<VethInterface>> {
    let kobject: Arc<dyn KObject> = iface;
    kobject.downcast_arc::<VethInterface>()
}

pub(crate) fn as_bridge(iface: Arc<dyn Iface>) -> Option<Arc<BridgeIface>> {
    let kobject: Arc<dyn KObject> = iface;
    kobject.downcast_arc::<BridgeIface>()
}

type VethPair = (Arc<dyn Iface>, Arc<dyn Iface>);

pub(crate) fn create_bridge(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    name: &str,
) -> Result<Arc<dyn Iface>, SystemError> {
    let bridge = BridgeIface::new(name);
    let iface: Arc<dyn Iface> = bridge;
    register_netdevice_locked(rtnl, netns, iface.clone())?;
    Ok(iface)
}

pub(crate) fn create_veth(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    name: &str,
    peer_netns: &Arc<NetNamespace>,
    peer_name: &str,
) -> Result<VethPair, SystemError> {
    if Arc::ptr_eq(netns, peer_netns) && name == peer_name {
        return Err(SystemError::EEXIST);
    }
    let (first, peer) = VethInterface::new_pair_dynamic(name, peer_name);
    let peer_iface: Arc<dyn Iface> = peer;
    let first_iface: Arc<dyn Iface> = first;
    let same_namespace = Arc::ptr_eq(netns, peer_netns);
    let first_addition = if same_namespace {
        netns.prepare_add_devices_locked(
            rtnl,
            &[(peer_iface.clone(), peer_name), (first_iface.clone(), name)],
        )?
    } else {
        netns.prepare_add_devices_locked(rtnl, &[(first_iface.clone(), name)])?
    };
    let peer_addition = if same_namespace {
        None
    } else {
        Some(peer_netns.prepare_add_devices_locked(rtnl, &[(peer_iface.clone(), peer_name)])?)
    };
    // Linux veth_newlink registers the peer first, then the outer device.
    netdev_register_kobject(peer_netns, peer_iface.clone())?;
    if let Err(error) = netdev_register_kobject(netns, first_iface.clone()) {
        netdev_unregister_kobject(peer_iface);
        return Err(error);
    }
    for iface in [&peer_iface, &first_iface] {
        iface
            .smol_iface()
            .lock()
            .set_route_table_includes_connected_prefixes(true);
        iface.set_net_state(NetDeivceState::__LINK_STATE_PRESENT);
    }
    if let Some(peer_addition) = peer_addition {
        peer_addition.publish();
    }
    first_addition.publish();
    for iface in [&peer_iface, &first_iface] {
        if let Some(napi) = iface.napi_struct() {
            napi_enable(&napi);
        }
        netdev_emit_uevent(iface.clone(), "add");
    }
    Ok((first_iface, peer_iface))
}

/// Undo a just-created link when parsing or initial settings fail before the
/// RTM_NEWLINK caller receives an ACK. Unlike ordinary deletion this path is
/// deliberately route-free and cannot allocate after the first quiesce.
pub(crate) fn rollback_new_link_unrouted(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: Arc<dyn Iface>,
) {
    if let Some(bridge) = as_bridge(iface.clone()) {
        assert_new_link_unrouted(netns, &iface);
        bridge
            .detach_all_ports()
            .expect("unannounced bridge must have consistent port membership");
        quiesce_unrouted(&iface);
        netns.remove_unrouted_devices_locked(rtnl, core::slice::from_ref(&iface));
        finish_unrouted_unregister(iface);
        return;
    }
    let veth = as_veth(iface.clone()).expect("creation rollback only supports bridge/veth");
    let peer = veth
        .peer_veth_opt()
        .expect("unannounced veth must retain its peer");
    let peer_owner = peer
        .net_namespace()
        .expect("unannounced veth peer must remain registered");
    let peer_iface: Arc<dyn Iface> = peer.clone();
    assert_new_link_unrouted(netns, &iface);
    assert_new_link_unrouted(&peer_owner, &peer_iface);
    assert!(
        veth.bridge_master().is_none() && peer.bridge_master().is_none(),
        "creation rollback runs before any master attachment is published"
    );
    veth.shutdown_pair();
    if Arc::ptr_eq(netns, &peer_owner) {
        netns.remove_unrouted_devices_locked(rtnl, &[iface.clone(), peer_iface.clone()]);
    } else {
        netns.remove_unrouted_devices_locked(rtnl, core::slice::from_ref(&iface));
        peer_owner.remove_unrouted_devices_locked(rtnl, core::slice::from_ref(&peer_iface));
    }
    finish_unrouted_unregister(iface);
    finish_unrouted_unregister(peer_iface);
}

fn assert_new_link_unrouted(netns: &Arc<NetNamespace>, iface: &Arc<dyn Iface>) {
    assert!(
        iface
            .net_namespace()
            .is_some_and(|owner| Arc::ptr_eq(&owner, netns)),
        "creation rollback requires the original namespace ownership"
    );
    let router = netns.router();
    assert!(
        !router.fib.read().has_oif(iface.nic_id() as u32),
        "unannounced dynamic link unexpectedly acquired FIB routes"
    );
}

fn quiesce_unrouted(iface: &Arc<dyn Iface>) {
    iface.common().close_tx_and_wait();
    iface.begin_admin_down();
    if let Some(napi) = iface.napi_struct() {
        crate::driver::net::napi::napi_pause_and_wait(&napi);
    }
    iface.quiesce_admin_down();
}

fn finish_unrouted_unregister(iface: Arc<dyn Iface>) {
    if let Some(napi) = iface.napi_struct() {
        if !crate::driver::net::napi::napi_is_disabled(&napi) {
            crate::driver::net::napi::napi_disable(&napi);
        }
    }
    iface.common().retire_for_device_removal();
    netdev_emit_uevent(iface.clone(), "remove");
    iface.clear_net_state(NetDeivceState::__LINK_STATE_PRESENT);
    netdev_unregister_kobject(iface.clone());
    iface.clear_net_namespace();
}

pub(crate) fn delete_link(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: Arc<dyn Iface>,
) -> Result<(), SystemError> {
    if let Some(bridge) = as_bridge(iface.clone()) {
        let prepared =
            prepare_unregister_netdevices_locked(rtnl, netns, core::slice::from_ref(&iface))?;
        let ports = bridge.prepare_detach_all_ports(rtnl)?;
        ports.commit();
        prepared.quiesce();
        prepared.publish_quiesced();
        return Ok(());
    }
    let Some(veth) = as_veth(iface.clone()) else {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    };
    let peer = veth.peer_veth_opt();
    let peer_owner = peer.as_ref().and_then(|peer| peer.net_namespace());
    let same_namespace = peer_owner
        .as_ref()
        .is_some_and(|owner| Arc::ptr_eq(owner, netns));
    let first_master = veth.bridge_master();
    let peer_master = peer.as_ref().and_then(|peer| peer.bridge_master());
    let shared_master = first_master.as_ref().is_some_and(|first| {
        peer_master
            .as_ref()
            .is_some_and(|second| Arc::ptr_eq(first, second))
    });
    let first_bridge = if let Some(master) = first_master.as_ref() {
        if shared_master {
            let peer = peer.as_ref().ok_or(SystemError::ENODEV)?;
            Some(master.prepare_remove_ports(rtnl, netns, &[veth.clone(), peer.clone()], None)?)
        } else {
            Some(master.prepare_remove_ports(rtnl, netns, core::slice::from_ref(&veth), None)?)
        }
    } else {
        None
    };
    let peer_bridge = if shared_master {
        None
    } else if let (Some(peer), Some(master), Some(owner)) =
        (peer.as_ref(), peer_master.as_ref(), peer_owner.as_ref())
    {
        let staged = if same_namespace {
            first_bridge.as_ref().and_then(|plan| plan.staged_fib())
        } else {
            None
        };
        Some(master.prepare_remove_ports(rtnl, owner, core::slice::from_ref(peer), staged)?)
    } else {
        None
    };
    let source_staged = if same_namespace {
        peer_bridge
            .as_ref()
            .and_then(|plan| plan.staged_fib())
            .or_else(|| first_bridge.as_ref().and_then(|plan| plan.staged_fib()))
    } else {
        first_bridge.as_ref().and_then(|plan| plan.staged_fib())
    };
    let prepared = if same_namespace {
        let peer_iface: Arc<dyn Iface> = peer.as_ref().unwrap().clone();
        prepare_unregister_netdevices_from_locked(
            rtnl,
            netns,
            &[iface.clone(), peer_iface],
            source_staged,
        )?
    } else {
        prepare_unregister_netdevices_from_locked(
            rtnl,
            netns,
            core::slice::from_ref(&iface),
            source_staged,
        )?
    };
    let peer_prepared = if let (Some(peer), Some(owner)) = (peer.as_ref(), peer_owner.as_ref()) {
        if same_namespace {
            None
        } else {
            let peer_iface: Arc<dyn Iface> = peer.clone();
            Some(prepare_unregister_netdevices_from_locked(
                rtnl,
                owner,
                core::slice::from_ref(&peer_iface),
                peer_bridge.as_ref().and_then(|plan| plan.staged_fib()),
            )?)
        }
    } else {
        None
    };
    if let Some(plan) = first_bridge {
        if let Some(commit) = plan.commit() {
            crate::net::socket::netlink::notify_link_commit(netns, commit);
        }
    }
    if let Some(plan) = peer_bridge {
        let owner = peer_owner.as_ref().ok_or(SystemError::ENODEV)?;
        if let Some(commit) = plan.commit() {
            crate::net::socket::netlink::notify_link_commit(owner, commit);
        }
    }
    veth.shutdown_pair();
    prepared.publish_quiesced();
    if let Some(peer_prepared) = peer_prepared {
        peer_prepared.publish_quiesced();
    }
    Ok(())
}

/// The final netns reference may be dropped by NAPI itself. Its devices are
/// therefore passed to a worker, and this helper only tears down edges that
/// outlive that namespace; the worker later quiesces its own detached devices.
pub(crate) fn teardown_detached_netns_device(rtnl: &RtnlGuard, iface: Arc<dyn Iface>) {
    if let Some(bridge) = as_bridge(iface.clone()) {
        bridge
            .detach_all_ports()
            .expect("dying bridge must have consistent port membership");
    }
    let Some(veth) = as_veth(iface) else {
        return;
    };
    let peer = veth.peer_veth_opt();
    let peer_owner = peer.as_ref().and_then(|peer| peer.net_namespace());
    let peer_deleted = peer
        .as_ref()
        .filter(|_| peer_owner.is_some())
        .map(|peer| {
            let iface: Arc<dyn Iface> = peer.clone();
            crate::net::socket::netlink::prepare_link_delete(&iface)
        })
        .transpose();
    let peer_deleted = match peer_deleted {
        Ok(message) => message,
        Err(error) => {
            log::warn!(
                "netns teardown: DELLINK notification unavailable: {:?}",
                error
            );
            None
        }
    };
    veth.shutdown_pair();
    // The dying netns has no FIB left. All of these steps only remove
    // existing state, so cleanup cannot fail for lack of memory.
    if let Some(master) = veth.bridge_master() {
        master
            .detach_port_without_netns(veth.clone())
            .expect("dying netns veth must remain attached to its bridge");
    }
    if let (Some(peer), Some(owner)) = (peer, peer_owner) {
        let peer_master = peer.bridge_master();
        let old_bridge_carrier = peer_master.as_ref().map(|bridge| {
            !bridge
                .net_state()
                .contains(NetDeivceState::__LINK_STATE_NOCARRIER)
        });
        if let Some(master) = peer_master.as_ref() {
            master
                .detach_port_for_netns_teardown(peer.clone())
                .expect("live peer must remain attached to its bridge");
        }
        let peer_iface: Arc<dyn Iface> = peer;
        crate::net::route::purge_iface_for_netns_teardown(&owner, &peer_iface);
        owner.remove_unrouted_devices_locked(rtnl, core::slice::from_ref(&peer_iface));
        finish_unrouted_unregister(peer_iface);
        if let (Some(bridge), Some(had_carrier)) = (peer_master, old_bridge_carrier) {
            let has_carrier = !bridge
                .net_state()
                .contains(NetDeivceState::__LINK_STATE_NOCARRIER);
            if had_carrier != has_carrier {
                let iface: Arc<dyn Iface> = bridge;
                crate::net::socket::netlink::notify_link_change(&iface);
            }
        }
        if let Some(deleted) = peer_deleted {
            crate::net::socket::netlink::notify_link_delete(owner, deleted);
        }
    }
}

pub(crate) fn set_master(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: Arc<dyn Iface>,
    master: Option<Arc<dyn Iface>>,
) -> Result<(), SystemError> {
    let veth = as_veth(iface).ok_or(SystemError::EOPNOTSUPP_OR_ENOTSUP)?;
    if !veth
        .net_namespace()
        .is_some_and(|owner| Arc::ptr_eq(&owner, netns))
    {
        return Err(SystemError::ENODEV);
    }
    match master {
        Some(master) => {
            let bridge = as_bridge(master).ok_or(SystemError::EINVAL)?;
            if !bridge
                .net_namespace()
                .is_some_and(|owner| Arc::ptr_eq(&owner, netns))
            {
                return Err(SystemError::EINVAL);
            }
            if let Some(commit) = bridge.add_port(rtnl, netns, veth)? {
                crate::net::socket::netlink::notify_link_commit(netns, commit);
            }
            Ok(())
        }
        None => {
            let bridge = veth.bridge_master().ok_or(SystemError::EINVAL)?;
            if let Some(commit) = bridge.remove_port(rtnl, netns, veth)? {
                crate::net::socket::netlink::notify_link_commit(netns, commit);
            }
            Ok(())
        }
    }
}

pub(crate) fn move_veth(
    rtnl: &RtnlGuard,
    source: &Arc<NetNamespace>,
    target: &Arc<NetNamespace>,
    iface: Arc<dyn Iface>,
    new_name: Option<&str>,
) -> Result<(), SystemError> {
    let veth = as_veth(iface.clone()).ok_or(SystemError::EINVAL)?;
    if !iface
        .net_namespace()
        .is_some_and(|owner| Arc::ptr_eq(&owner, source))
    {
        return Err(SystemError::ENODEV);
    }
    if Arc::ptr_eq(source, target) {
        return Ok(());
    }
    if veth.bridge_master().is_some() {
        return Err(SystemError::EBUSY);
    }
    let old_name = iface.iface_name();
    let destination_name = new_name.unwrap_or(old_name.as_str());
    validate_name_bytes(destination_name)?;
    if target.device_list().values().any(|other| {
        other
            .common()
            .with_iface_name(|name| name == destination_name)
    }) {
        return Err(SystemError::EEXIST);
    }
    // Prepare every fallible source, destination, and sysfs step while the
    // interface still belongs to the source namespace.
    let mut destination_name_owned = alloc::string::String::new();
    destination_name_owned
        .try_reserve_exact(destination_name.len())
        .map_err(|_| SystemError::ENOMEM)?;
    destination_name_owned.push_str(destination_name);
    let mut sysfs_name = alloc::string::String::new();
    sysfs_name
        .try_reserve_exact(destination_name.len())
        .map_err(|_| SystemError::ENOMEM)?;
    sysfs_name.push_str(destination_name);
    let source_removal =
        prepare_unregister_netdevices_locked(rtnl, source, core::slice::from_ref(&iface))?;
    let target_addition =
        target.prepare_add_devices_locked(rtnl, &[(iface.clone(), destination_name)])?;
    let sockets_move = iface.common().prepare_socket_set_move()?;
    let stack_move = veth.prepare_stack_for_netns_move()?;
    let sysfs_move = prepare_netdev_sysfs_move(&iface, sysfs_name, target)?;

    // Linux closes a running netdevice before moving it to another netns.
    // Prepare the configured flag change before the first irreversible step.
    let down_flags = iface.common().prepare_configured_flags(
        crate::driver::net::types::InterfaceFlags::empty(),
        crate::driver::net::types::InterfaceFlags::UP,
    )?;
    // This is the final fallible step. Kernfs validates both old and target
    // keys and reserves both target buckets before changing either inode.
    let old_devpath = sysfs_move
        .commit()?
        .expect("registered netdevice must have a sysfs path");
    source_removal.quiesce();
    iface.common().begin_netns_move();
    iface.common().wait_for_poll_before_netns_move();
    iface.common().retire_queued_work_for_netns_move();
    source_removal.publish_moving();
    sockets_move.publish(iface.common());
    detach_for_netns_move(&iface);
    veth.publish_stack_after_netns_move(stack_move);
    iface.common().set_name(destination_name_owned);
    target_addition.publish();
    iface.common().end_netns_move();
    iface.common().activate_queued_work_after_netns_move();
    reactivate_moved_device(&iface, down_flags);
    crate::driver::net::sysfs::netdev_emit_move_uevent(iface, old_devpath);
    Ok(())
}

fn reactivate_moved_device(
    iface: &Arc<dyn Iface>,
    down_flags: crate::driver::net::PreparedConfiguredFlags,
) {
    iface.publish_admin_state(false);
    iface.common().publish_configured_flags(down_flags);
    iface.clear_net_state(NetDeivceState::__LINK_STATE_START);
    iface.set_operstate(crate::driver::net::Operstate::IF_OPER_DOWN);
    if let Some(napi) = iface.napi_struct() {
        napi_resume(napi);
    }
}
