use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use system_error::SystemError;

use crate::filesystem::ramfs::RamFS;
use crate::filesystem::vfs::{
    mount::{MountFlags, MOUNT_LIFECYCLE_LOCK},
    MountFS,
};

use super::{change::*, event::*, group::*, state::*};

#[test]
fn test_propagation_type_default() {
    let prop = MountPropagation::new_private();
    assert!(prop.is_private());
    assert!(!prop.is_shared());
}

#[test]
fn test_propagation_type_change() {
    let prop = MountPropagation::new_private();
    prop.set_shared().unwrap();
    assert!(prop.is_shared());
    assert!(prop.peer_group_id().is_valid());

    prop.set_private();
    assert!(prop.is_private());

    prop.set_unbindable();
    assert!(prop.is_unbindable());
}

#[test]
fn test_propagation_type_flags_match_linux_validation() {
    assert_eq!(
        flags_to_propagation_type(MountFlags::SHARED | MountFlags::REC | MountFlags::SILENT),
        Ok(PropagationType::Shared)
    );
    assert_eq!(
        flags_to_propagation_type(MountFlags::SHARED | MountFlags::PRIVATE),
        Err(SystemError::EINVAL)
    );
    assert_eq!(
        flags_to_propagation_type(MountFlags::SHARED | MountFlags::NODEV),
        Err(SystemError::EINVAL)
    );
    assert_eq!(
        flags_to_propagation_type(MountFlags::REC | MountFlags::SILENT),
        Err(SystemError::EINVAL)
    );
}

#[test]
fn test_propagation_group_allocator_reuses_freed_holes() {
    let mut allocator = PropagationGroupIdAllocator::new();
    let first = allocator.alloc().unwrap();
    let second = allocator.alloc().unwrap();
    assert_eq!(second, first + 1);

    allocator.free(first);
    assert_eq!(allocator.alloc(), Some(first));
    assert_eq!(allocator.reusable_count, 0);
    assert_eq!(allocator.lowest_free, allocator.next_fresh);
    assert_eq!(allocator.alloc(), Some(second + 1));
}

fn new_test_mount(propagation: Arc<MountPropagation>) -> Arc<MountFS> {
    let mount = MountFS::new(
        RamFS::new(),
        None,
        None,
        propagation,
        None,
        MountFlags::empty(),
        None,
    );
    // Test fixtures represent mounts that have already been published.
    // Production constructors intentionally remain in Constructing until
    // their topology/namespace insertion commits.
    mount.activate().unwrap();
    mount
}

fn attach_test_child(parent: &Arc<MountFS>, child: &Arc<MountFS>) {
    let mountpoint = parent.mountpoint_root_inode();
    child.set_self_mountpoint(Some(mountpoint.clone()));
    parent.attach_top(&mountpoint, child.clone()).unwrap();
}

fn shared_copy(source: &Arc<MountFS>) -> Arc<MountFS> {
    let copy = source.deepcopy(None).unwrap();
    copy.activate().unwrap();
    copy
}

#[test]
fn test_recursive_target_collection_is_dfs_preorder() {
    let root = new_test_mount(MountPropagation::new_private());
    let child_a = new_test_mount(MountPropagation::new_private());
    let child_b = new_test_mount(MountPropagation::new_private());
    let grandchild_a = new_test_mount(MountPropagation::new_private());
    let grandchild_b = new_test_mount(MountPropagation::new_private());
    attach_test_child(&root, &child_a);
    attach_test_child(&root, &child_b);
    attach_test_child(&child_a, &grandchild_a);
    attach_test_child(&child_b, &grandchild_b);

    let targets = collect_change_targets(&root, true, &mut || Ok(())).unwrap();
    let position = |mount: &Arc<MountFS>| {
        targets
            .iter()
            .position(|target| Arc::ptr_eq(target, mount))
            .unwrap()
    };
    assert_eq!(position(&root), 0);
    assert_eq!(position(&grandchild_a), position(&child_a) + 1);
    assert_eq!(position(&grandchild_b), position(&child_b) + 1);
}

#[test]
fn test_peer_ring_scan_starts_after_middle_target() {
    let peer_a = new_test_mount(MountPropagation::new_shared().unwrap());
    let group = peer_a.propagation().peer_group().unwrap();
    let target_b = shared_copy(&peer_a);
    let peer_c = shared_copy(&peer_a);
    let fallback_d = new_test_mount(MountPropagation::new_shared_with_group(group));
    let group_id = peer_a.propagation().peer_group_id();
    for mount in [&peer_a, &target_b, &peer_c, &fallback_d] {
        register_peer(group_id, mount);
    }

    let ring = get_peers(group_id, &target_b);
    assert!(Arc::ptr_eq(&ring[0], &peer_c));
    change_mnt_propagation(&target_b, PropagationType::Slave).unwrap();
    assert!(
        target_b
            .propagation()
            .master()
            .is_some_and(|master| Arc::ptr_eq(&master, &peer_c))
    );
}

#[test]
fn test_recursive_prepare_group_failure_changes_nothing() {
    let root = new_test_mount(MountPropagation::new_private());
    let child = new_test_mount(MountPropagation::new_private());
    attach_test_child(&root, &child);

    let calls = Cell::new(0);
    let first_group = RefCell::new(None);
    let result = PropagationChangeTransaction::prepare(
        &root,
        PropagationType::Shared,
        true,
        || {
            let call = calls.get();
            calls.set(call + 1);
            if call == 1 {
                return Err(SystemError::ENOSPC);
            }
            let group = PropagationGroup::alloc()?;
            *first_group.borrow_mut() = Some(Arc::downgrade(&group));
            Ok(group)
        },
        || Ok(()),
    );

    assert!(matches!(result, Err(SystemError::ENOSPC)));
    assert!(root.propagation().is_private());
    assert!(child.propagation().is_private());
    assert!(
        first_group
            .borrow()
            .as_ref()
            .is_some_and(|group| group.upgrade().is_none())
    );
}

#[test]
fn test_recursive_prepare_capacity_failure_changes_nothing() {
    let root = new_test_mount(MountPropagation::new_private());
    let group_weak = RefCell::new(None);
    let fail_next_reserve = Cell::new(false);
    let result = PropagationChangeTransaction::prepare(
        &root,
        PropagationType::Shared,
        false,
        || {
            let group = PropagationGroup::alloc()?;
            *group_weak.borrow_mut() = Some(Arc::downgrade(&group));
            fail_next_reserve.set(true);
            Ok(group)
        },
        || {
            if fail_next_reserve.replace(false) {
                Err(SystemError::ENOMEM)
            } else {
                Ok(())
            }
        },
    );

    assert!(matches!(result, Err(SystemError::ENOMEM)));
    assert!(root.propagation().is_private());
    assert!(
        group_weak
            .borrow()
            .as_ref()
            .is_some_and(|group| group.upgrade().is_none())
    );
}

#[test]
fn test_group_allocator_reuses_multiple_holes_in_minimum_order_without_free_storage() {
    let mut allocator = PropagationGroupIdAllocator::new();
    let ids: Vec<_> = (0..6).map(|_| allocator.alloc().unwrap()).collect();
    allocator.free(ids[4]);
    allocator.free(0);
    assert_eq!(allocator.lowest_free, ids[4]);
    allocator.free(ids[1]);
    allocator.free(ids[3]);

    assert_eq!(allocator.lowest_free, ids[1]);
    assert_eq!(allocator.alloc(), Some(ids[1]));
    assert_eq!(allocator.alloc(), Some(ids[3]));
    assert_eq!(allocator.alloc(), Some(ids[4]));
    assert_eq!(allocator.reusable_count, 0);
    assert_eq!(allocator.lowest_free, allocator.next_fresh);
    assert_eq!(allocator.alloc(), Some(ids[5] + 1));
}

#[test]
fn test_recursive_slave_chain_materializes_each_final_edge_once() {
    let mount_a = new_test_mount(MountPropagation::new_shared().unwrap());
    let group_id = mount_a.propagation().peer_group_id();
    let mount_b = shared_copy(&mount_a);
    let mount_c = shared_copy(&mount_a);
    let external = shared_copy(&mount_a);
    attach_test_child(&mount_a, &mount_b);
    attach_test_child(&mount_b, &mount_c);
    for mount in [&mount_a, &mount_b, &mount_c, &external] {
        register_peer(group_id, mount);
    }

    change_mnt_propagation_recursive(&mount_a, PropagationType::Slave, true).unwrap();

    let external_slaves = external.propagation().slaves();
    assert_eq!(external_slaves.len(), 3);
    for mount in [&mount_a, &mount_b, &mount_c] {
        assert!(!mount.propagation().is_shared());
        assert!(
            mount
                .propagation()
                .master()
                .is_some_and(|master| Arc::ptr_eq(&master, &external))
        );
        assert_eq!(
            external_slaves
                .iter()
                .filter(|slave| Arc::ptr_eq(slave, mount))
                .count(),
            1
        );
    }
}

#[test]
fn test_make_slave_preserves_new_masters_unrelated_slave() {
    let target = new_test_mount(MountPropagation::new_shared().unwrap());
    let group_id = target.propagation().peer_group_id();
    let peer = shared_copy(&target);
    let unrelated = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&peer)));
    peer.propagation().add_slave(Arc::downgrade(&unrelated));
    register_peer(group_id, &target);
    register_peer(group_id, &peer);

    change_mnt_propagation(&target, PropagationType::Slave).unwrap();

    let slaves = peer.propagation().slaves();
    assert_eq!(slaves.len(), 2);
    assert!(Arc::ptr_eq(&slaves[0], &target));
    assert!(Arc::ptr_eq(&slaves[1], &unrelated));
    assert!(
        unrelated
            .propagation()
            .master()
            .is_some_and(|master| Arc::ptr_eq(&master, &peer))
    );
}

#[test]
fn test_nonshared_change_reparents_existing_slave_subtree_like_linux() {
    let master = new_test_mount(MountPropagation::new_private());
    let target = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&master)));
    let child = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&target)));
    master.propagation().add_slave(Arc::downgrade(&target));
    target.propagation().add_slave(Arc::downgrade(&child));

    change_mnt_propagation(&target, PropagationType::Slave).unwrap();
    assert!(
        target
            .propagation()
            .master()
            .is_some_and(|current| Arc::ptr_eq(&current, &master))
    );
    assert!(
        child
            .propagation()
            .master()
            .is_some_and(|current| Arc::ptr_eq(&current, &master))
    );
    assert!(target.propagation().slaves().is_empty());

    change_mnt_propagation(&target, PropagationType::Private).unwrap();
    assert!(target.propagation().master().is_none());
    assert!(
        child
            .propagation()
            .master()
            .is_some_and(|current| Arc::ptr_eq(&current, &master))
    );
    assert!(target.propagation().slaves().is_empty());
}

#[test]
fn test_make_slave_prefers_exact_root_dentry_peer() {
    let target = new_test_mount(MountPropagation::new_shared().unwrap());
    let group = target.propagation().peer_group().unwrap();
    let fallback = new_test_mount(MountPropagation::new_shared_with_group(group));
    let exact = shared_copy(&target);
    let group_id = target.propagation().peer_group_id();
    register_peer(group_id, &target);
    register_peer(group_id, &fallback);
    register_peer(group_id, &exact);

    assert!(!Arc::ptr_eq(&target.root_dentry(), &fallback.root_dentry()));
    assert!(Arc::ptr_eq(&target.root_dentry(), &exact.root_dentry()));
    change_mnt_propagation(&target, PropagationType::Slave).unwrap();

    assert!(
        target
            .propagation()
            .master()
            .is_some_and(|master| Arc::ptr_eq(&master, &exact))
    );
}

#[test]
fn test_make_slave_selects_peer_as_master() {
    let prop_a = MountPropagation::new_shared().unwrap();
    let group_id = prop_a.peer_group_id();
    let prop_b = MountPropagation::new_shared_with_group(prop_a.peer_group().unwrap());
    let mount_a = new_test_mount(prop_a);
    let mount_b = new_test_mount(prop_b);

    register_peer(group_id, &mount_a);
    register_peer(group_id, &mount_b);

    change_mnt_propagation(&mount_a, PropagationType::Slave).unwrap();

    let prop_a = mount_a.propagation();
    assert!(prop_a.is_slave());
    assert!(!prop_a.is_shared());
    assert!(
        prop_a
            .master()
            .is_some_and(|master| Arc::ptr_eq(&master, &mount_b))
    );
    assert!(
        mount_b
            .propagation()
            .slaves()
            .iter()
            .any(|slave| Arc::ptr_eq(slave, &mount_a))
    );
}

#[test]
fn test_make_shared_preserves_slave_master() {
    let master = new_test_mount(MountPropagation::new_shared().unwrap());
    let slave = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&master)));
    master.propagation().add_slave(Arc::downgrade(&slave));

    change_mnt_propagation(&slave, PropagationType::Shared).unwrap();

    let slave_prop = slave.propagation();
    assert!(slave_prop.is_shared());
    assert!(slave_prop.is_slave());
    assert!(slave_prop.info_string().contains("shared:"));
    assert!(slave_prop.info_string().contains("master:"));
}

#[test]
fn test_make_slave_without_master_orphans_existing_slaves() {
    let parent = new_test_mount(MountPropagation::new_private());
    let child = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&parent)));
    parent.propagation().add_slave(Arc::downgrade(&child));

    change_mnt_propagation(&parent, PropagationType::Slave).unwrap();

    assert!(parent.propagation().is_private());
    assert!(!child.propagation().is_slave());
    assert!(parent.propagation().slaves().is_empty());
}

#[test]
fn test_slaves_prunes_stale_entries() {
    let master = new_test_mount(MountPropagation::new_private());
    {
        let child = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&master)));
        master.propagation().add_slave(Arc::downgrade(&child));
        assert_eq!(master.propagation().slaves().len(), 1);
    }

    assert!(master.propagation().slaves().is_empty());
}

#[test]
fn test_umount_at_peer_detaches_slave_from_master() {
    let source_child = new_test_mount(MountPropagation::new_private());
    let peer = new_test_mount(MountPropagation::new_private());
    let child = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&source_child)));
    let mountpoint = peer.mountpoint_root_inode();

    source_child.propagation().add_slave(Arc::downgrade(&child));
    child.set_self_mountpoint(Some(mountpoint.clone()));
    peer.attach_top(&mountpoint, child.clone()).unwrap();

    umount_at_peer(&peer, &mountpoint, &source_child).unwrap();

    assert!(peer.lookup_top(&mountpoint).is_none());
    assert!(!child.propagation().is_slave());
    assert!(source_child.propagation().slaves().is_empty());
}

#[test]
fn test_propagate_to_shared_slave_keeps_child_shared_slave() {
    let master = new_test_mount(MountPropagation::new_shared().unwrap());
    register_peer(master.propagation().peer_group_id(), &master);
    let source_child = new_test_mount(MountPropagation::new_shared().unwrap());
    let source_child_group = source_child.propagation().peer_group_id();

    let slave_group = PropagationGroup::alloc().unwrap();
    let slave_a = master.deepcopy(None).unwrap();
    slave_a.propagation().set_private();
    slave_a
        .propagation()
        .set_slave(Some(Arc::downgrade(&master)));
    slave_a
        .propagation()
        .set_shared_with_group(slave_group.clone());
    slave_a.activate().unwrap();
    let slave_b = master.deepcopy(None).unwrap();
    slave_b.propagation().set_private();
    slave_b
        .propagation()
        .set_slave(Some(Arc::downgrade(&master)));
    slave_b
        .propagation()
        .set_shared_with_group(slave_group.clone());
    slave_b.activate().unwrap();

    register_peer(slave_group.id(), &slave_a);
    register_peer(slave_group.id(), &slave_b);
    master.propagation().add_slave(Arc::downgrade(&slave_a));
    master.propagation().add_slave(Arc::downgrade(&slave_b));

    let mountpoint = master.mountpoint_root_inode();
    source_child.set_self_mountpoint(Some(mountpoint.clone()));
    let _topology = MOUNT_LIFECYCLE_LOCK.lock();
    let prepared = prepare_mount_propagation_locked(&master, &mountpoint, &source_child).unwrap();
    master
        .attach_top(&mountpoint, source_child.clone())
        .unwrap();
    commit_mount_propagation_locked(prepared).unwrap();

    let child_a = slave_a
        .lookup_top(
            &slave_a
                .wrapper_for_dentry(mountpoint.shared_dentry())
                .unwrap(),
        )
        .expect("slave_a should receive propagated child");
    let child_b = slave_b
        .lookup_top(
            &slave_b
                .wrapper_for_dentry(mountpoint.shared_dentry())
                .unwrap(),
        )
        .expect("slave_b should receive propagated child");
    let child_a_prop = child_a.propagation();
    let child_b_prop = child_b.propagation();

    assert!(child_a_prop.is_slave());
    assert!(child_a_prop.is_shared());
    assert!(child_b_prop.is_slave());
    assert!(child_b_prop.is_shared());
    assert_eq!(child_a_prop.peer_group_id(), child_b_prop.peer_group_id());
    assert_ne!(child_a_prop.peer_group_id(), source_child_group);
}

#[test]
fn test_nearest_propagated_source_skips_unmaterialized_master() {
    let master = new_test_mount(MountPropagation::new_shared().unwrap());
    let skipped = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&master)));
    let deep = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&skipped)));
    master.propagation().add_slave(Arc::downgrade(&skipped));
    skipped.propagation().add_slave(Arc::downgrade(&deep));

    let source_child = new_test_mount(MountPropagation::new_private());
    let mut propagated_sources = CorrespondingSources::new();
    propagated_sources.insert(&master, source_child.clone());

    let selected = propagated_sources
        .nearest(&deep)
        .unwrap()
        .expect("a deeper slave must fall back to the nearest materialized master");
    assert!(Arc::ptr_eq(&selected, &source_child));
}

#[test]
fn test_uncovered_slave_does_not_prune_covered_deeper_slave() {
    let master = new_test_mount(MountPropagation::new_shared().unwrap());
    register_peer(master.propagation().peer_group_id(), &master);

    // The uncovered peer models a narrow bind root in the source peer group.
    // The deeper slave deliberately has the master's wider object view, so
    // Linux keeps the peer layer's last source and still propagates to it.
    let group = master.propagation().peer_group().unwrap();
    let skipped = new_test_mount(MountPropagation::new_shared_with_group(group));
    register_peer(master.propagation().peer_group_id(), &skipped);
    let deep = master.deepcopy(None).unwrap();
    deep.propagation().set_private();
    deep.propagation().set_slave(Some(Arc::downgrade(&skipped)));
    deep.activate().unwrap();
    skipped.propagation().add_slave(Arc::downgrade(&deep));

    let mountpoint = master.mountpoint_root_inode();
    assert!(matches!(
        skipped.wrapper_for_dentry(mountpoint.shared_dentry()),
        Err(SystemError::EXDEV)
    ));
    let deep_mountpoint = deep.wrapper_for_dentry(mountpoint.shared_dentry()).unwrap();
    let source_child = new_test_mount(MountPropagation::new_private());
    source_child.set_self_mountpoint(Some(mountpoint.clone()));

    let _topology = MOUNT_LIFECYCLE_LOCK.lock();
    let prepared = prepare_mount_propagation_locked(&master, &mountpoint, &source_child).unwrap();
    master
        .attach_top(&mountpoint, source_child.clone())
        .unwrap();
    commit_mount_propagation_locked(prepared).unwrap();

    assert!(skipped.mount_children().is_empty());
    let deep_child = deep
        .lookup_top(&deep_mountpoint)
        .expect("the covered deeper slave must receive the propagation event");
    assert!(
        deep_child
            .propagation()
            .master()
            .is_some_and(|source| Arc::ptr_eq(&source, &source_child))
    );
}

#[test]
fn test_move_propagation_is_prepared_before_source_edge_changes() {
    let old_parent = new_test_mount(MountPropagation::new_private());
    let moved_root = new_test_mount(MountPropagation::new_private());
    attach_test_child(&old_parent, &moved_root);
    let old_mountpoint = moved_root.self_mountpoint().unwrap();

    let target = new_test_mount(MountPropagation::new_shared().unwrap());
    let target_peer = shared_copy(&target);
    let target_group = target.propagation().peer_group_id();
    register_peer(target_group, &target);
    register_peer(target_group, &target_peer);
    let target_mountpoint = target.mountpoint_root_inode();

    let _topology = MOUNT_LIFECYCLE_LOCK.lock();
    let prepared =
        prepare_moved_tree_propagation_locked(&target, &moved_root, &target_mountpoint).unwrap();

    assert!(
        old_parent
            .children_at(&old_mountpoint)
            .iter()
            .any(|child| Arc::ptr_eq(child, &moved_root))
    );
    assert!(moved_root.propagation().is_shared());
    assert!(target.lookup_top(&target_mountpoint).is_none());
    assert!(
        target_peer
            .lookup_top(
                &target_peer
                    .wrapper_for_dentry(target_mountpoint.shared_dentry())
                    .unwrap()
            )
            .is_none()
    );

    abort_moved_tree_propagation_locked(prepared);
    assert!(moved_root.propagation().is_private());
    assert!(
        old_parent
            .children_at(&old_mountpoint)
            .iter()
            .any(|child| Arc::ptr_eq(child, &moved_root))
    );
}

#[test]
fn test_tuck_under_rollback_restores_cover_in_place() {
    let parent = new_test_mount(MountPropagation::new_private());
    let mountpoint = parent.mountpoint_root_inode();
    let covered = new_test_mount(MountPropagation::new_private());
    covered.set_self_mountpoint(Some(mountpoint.clone()));
    parent.attach_top(&mountpoint, covered.clone()).unwrap();

    let propagated = new_test_mount(MountPropagation::new_private());
    propagated.set_self_mountpoint(Some(mountpoint.clone()));
    let cover_mountpoint = propagated.mountpoint_root_inode();
    let _cover_reservation = propagated.reserve_mount_edge(&cover_mountpoint, 1).unwrap();

    parent
        .attach_beneath(&mountpoint, propagated.clone())
        .unwrap();
    assert!(Arc::ptr_eq(
        &parent.lookup_top(&mountpoint).unwrap(),
        &propagated
    ));
    assert!(covered.is_tucked_under());

    let removed = parent.detach_exact_restoring_cover(&propagated).unwrap();
    assert!(Arc::ptr_eq(&removed, &propagated));
    assert!(Arc::ptr_eq(
        &parent.lookup_top(&mountpoint).unwrap(),
        &covered
    ));
    assert!(!covered.is_tucked_under());
    assert!(propagated.mount_children().is_empty());
}
