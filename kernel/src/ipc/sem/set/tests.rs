use super::*;
use crate::ipc::sem::manager::SemManager;
use crate::ipc::sem::set::test_support::*;
use crate::{ipc::id::IpcIdAllocator, libs::wait_queue::Waiter};
use crate::{
    ipc::sem_undo::detach_sem_undo,
    process::{
        namespace::ipc_namespace::INIT_IPC_NAMESPACE, namespace::pid_namespace::INIT_PID_NAMESPACE,
        KernelStack, ProcessControlBlock, RawPid,
    },
};

#[test]
fn last_owner_replays_adjustment_with_clamp_and_removes_record() {
    let ipc_ns = test_ipc_ns();
    let semid = {
        let mut manager = ipc_ns.sem.lock();
        insert_test_set(&mut manager, SemKey::new(31), &[32766])
    };
    let (pcb, group) = test_pcb_with_group(&ipc_ns);
    group.insert_test_record(semid, &[4]);

    detach_sem_undo(&pcb);

    let manager = ipc_ns.sem.lock();
    assert_eq!(sem_values(&manager, semid), vec![SEMVMX]);
    assert_eq!(group.record_count_for_test(), 0);
    assert!(pcb.sem_undo_group().is_none());
}

#[test]
fn non_last_owner_does_not_replay() {
    let ipc_ns = test_ipc_ns();
    let semid = {
        let mut manager = ipc_ns.sem.lock();
        insert_test_set(&mut manager, SemKey::new(32), &[10])
    };
    let (owner_one, group) = test_pcb_with_group(&ipc_ns);
    let owner_two = ProcessControlBlock::new_idle(0, KernelStack::new().unwrap());
    let mut guard = owner_one
        .prepare_shared_sem_undo_attachment(&ipc_ns)
        .unwrap();
    guard.install_into(&owner_two);
    guard.disarm();
    group.insert_test_record(semid, &[4]);

    detach_sem_undo(&owner_one);

    assert_eq!(sem_values(&ipc_ns.sem.lock(), semid), vec![10]);
    assert_eq!(group.record_count_for_test(), 1);

    detach_sem_undo(&owner_two);

    assert_eq!(sem_values(&ipc_ns.sem.lock(), semid), vec![14]);
    assert_eq!(group.record_count_for_test(), 0);
}

#[test]
fn stale_full_semid_does_not_touch_reused_index() {
    let ipc_ns = test_ipc_ns();
    let old_semid = {
        let mut manager = ipc_ns.sem.lock();
        manager.reset_allocator_for_test(2);
        insert_test_set(&mut manager, SemKey::new(33), &[7])
    };
    let (pcb, group) = test_pcb_with_group(&ipc_ns);
    group.insert_test_record(old_semid, &[9]);

    let new_semid = {
        let mut manager = ipc_ns.sem.lock();
        insert_test_set(&mut manager, SemKey::new(34), &[5]);
        remove_test_set(&mut manager, old_semid);
        insert_test_set(&mut manager, SemKey::new(35), &[21])
    };
    assert_ne!(old_semid, new_semid);
    assert_eq!(
        old_semid.data() & IpcIdAllocator::IPC_ID_IDX_MASK,
        new_semid.data() & IpcIdAllocator::IPC_ID_IDX_MASK
    );

    detach_sem_undo(&pcb);

    assert_eq!(sem_values(&ipc_ns.sem.lock(), new_semid), vec![21]);
    assert_eq!(group.record_count_for_test(), 0);
}

#[test]
fn replay_updates_otime_and_rescans_waiter() {
    let ipc_ns = test_ipc_ns();
    let (semid, entry) = {
        let mut manager = ipc_ns.sem.lock();
        let semid = insert_test_set(&mut manager, SemKey::new(35), &[1, 2]);
        let set = manager.get_by_semid_checked_mut(semid).unwrap();
        set.sem_otime = -1;

        let (_waiter, waker) = Waiter::new_pair();
        let blocker = SemBlockedOp {
            semnum: 0,
            wait_type: SemWaitType::Zero,
            nowait: false,
        };
        let entry = Arc::new(SemQueueEntry::new(
            &[PosixSemBuf {
                sem_num: 0,
                sem_op: 0,
                sem_flg: 0,
            }],
            None,
            waker,
            blocker,
        ));
        set.enqueue_waiter(entry.clone());
        (semid, entry)
    };
    let (pcb, group) = test_pcb_with_group(&ipc_ns);
    let exiting_tgid = Pid::new_for_test(RawPid::new(4242), INIT_PID_NAMESPACE.clone());
    pcb.install_pid_identity_for_test(exiting_tgid.clone());
    group.insert_test_record(semid, &[-1, 1]);

    detach_sem_undo(&pcb);

    let mut manager = ipc_ns.sem.lock();
    let set = manager.get_by_semid_checked_mut(semid).unwrap();
    let values = [set.sems[0].val, set.sems[1].val];
    let waiter_pid_was_applied = set.sems[0].pid.is_none();
    let replay_pid_was_applied = set.sems[1]
        .pid
        .as_ref()
        .is_some_and(|pid| Arc::ptr_eq(pid, &exiting_tgid));
    let replay_pid_vnr = set.sems[1]
        .pid
        .as_ref()
        .map(|pid| pid.pid_nr_ns(&INIT_PID_NAMESPACE));
    let sem_otime = set.sem_otime;
    let waiters_are_empty = set.pending_is_empty();
    let completed_result = entry.completed_result();
    let record_count = group.record_count_for_test();

    for sem in &mut set.sems {
        sem.pid = None;
    }
    drop(manager);
    pcb.clear_pid_identity_for_test();
    exiting_tgid.clear_numbers_for_test();

    assert_eq!(values, [0, 3]);
    assert!(waiter_pid_was_applied);
    assert!(replay_pid_was_applied);
    assert_eq!(replay_pid_vnr, Some(RawPid::new(4242)));
    assert_ne!(sem_otime, -1);
    assert!(waiters_are_empty);
    assert_eq!(completed_result, Some(Ok(0)));
    assert_eq!(record_count, 0);
}

#[test]
fn replay_rescans_and_updates_otime_when_clamp_does_not_change_value() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(36), &[SEMVMX, SEMVMX]);
    let entry = {
        let set = manager.get_by_semid_checked_mut(semid).unwrap();
        set.sem_otime = -1;
        enqueue_test_waiter(
            set,
            &[PosixSemBuf {
                sem_num: 0,
                sem_op: -1,
                sem_flg: 0,
            }],
            SemBlockedOp {
                semnum: 0,
                wait_type: SemWaitType::Increase,
                nowait: false,
            },
        )
    };
    let exiting_tgid = Pid::new_for_test(RawPid::new(4243), INIT_PID_NAMESPACE.clone());

    manager.replay_sem_undo_adjustments(
        semid,
        &[1, 1],
        Some(exiting_tgid.clone()),
        &mut SemWakeBatch::default(),
    );

    let set = manager.get_by_semid_checked_mut(semid).unwrap();
    let values = [set.sems[0].val, set.sems[1].val];
    let replay_pid_was_applied = set.sems[1]
        .pid
        .as_ref()
        .is_some_and(|pid| Arc::ptr_eq(pid, &exiting_tgid));
    let sem_otime = set.sem_otime;
    let waiters_are_empty = set.pending_is_empty();
    let completed_result = entry.completed_result();
    for sem in &mut set.sems {
        sem.pid = None;
    }
    exiting_tgid.clear_numbers_for_test();

    assert_eq!(values, [SEMVMX - 1, SEMVMX]);
    assert!(replay_pid_was_applied);
    assert_ne!(sem_otime, -1);
    assert!(waiters_are_empty);
    assert_eq!(completed_result, Some(Ok(0)));
}

#[test]
fn valid_all_zero_record_still_updates_otime_and_rescans_queue() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(37), &[0, 5]);
    let entry = {
        let set = manager.get_by_semid_checked_mut(semid).unwrap();
        set.sem_otime = -1;
        enqueue_test_waiter(
            set,
            &[PosixSemBuf {
                sem_num: 0,
                sem_op: 0,
                sem_flg: 0,
            }],
            SemBlockedOp {
                semnum: 0,
                wait_type: SemWaitType::Zero,
                nowait: false,
            },
        )
    };
    let exiting_tgid = Pid::new_for_test(RawPid::new(4244), INIT_PID_NAMESPACE.clone());

    manager.replay_sem_undo_adjustments(
        semid,
        &[0, 0],
        Some(exiting_tgid.clone()),
        &mut SemWakeBatch::default(),
    );

    let set = manager.get_by_semid_checked_mut(semid).unwrap();
    let values = [set.sems[0].val, set.sems[1].val];
    let untouched_pid = set.sems[1].pid.is_none();
    let sem_otime = set.sem_otime;
    let waiters_are_empty = set.pending_is_empty();
    let completed_result = entry.completed_result();
    for sem in &mut set.sems {
        sem.pid = None;
    }
    exiting_tgid.clear_numbers_for_test();

    assert_eq!(values, [0, 5]);
    assert!(untouched_pid);
    assert_ne!(sem_otime, -1);
    assert!(waiters_are_empty);
    assert_eq!(completed_result, Some(Ok(0)));
}

#[test]
fn undo_registry_shrink_releases_empty_and_rechecks_concurrent_growth() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(152), &[0]);
    let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    let set = manager.get_by_semid_checked_mut(semid).unwrap();
    set.undo_groups.prepare(64).unwrap();
    set.ensure_undo_group_registered_prepared(&group, &mut SemUndoRegistry::default())
        .unwrap();
    let mut spare = SemUndoRegistry::default();
    let mut retired = SemUndoRegistry::default();
    let needed = set.shrink_undo_registry_prepared(&mut spare, &mut retired);
    assert_eq!(needed, 4);
    spare.prepare(needed).unwrap();
    // Simulate new associations arriving during unlocked preparation.
    let mut owners = Vec::new();
    for _ in 0..8 {
        let owner = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        set.ensure_undo_group_registered_prepared(&owner, &mut SemUndoRegistry::default())
            .unwrap();
        owners.push(owner);
    }
    assert!(set.shrink_undo_registry_prepared(&mut spare, &mut retired) > 0);
    assert_eq!(set.undo_groups.len(), 9);
    assert_eq!(set.undo_groups.capacity(), 64);
    for owner in &owners {
        set.unregister_undo_group(owner);
    }
    assert_eq!(
        set.shrink_undo_registry_prepared(&mut spare, &mut retired),
        0
    );
    assert_eq!(set.undo_groups.len(), 1);
    assert_eq!(set.undo_groups.capacity(), 4);
    assert_eq!(spare.capacity(), 64);
    // The spare is already allocated when another owner empties the set.
    // It must not be installed back into the empty registry.
    set.unregister_undo_group(&group);
    assert_eq!(
        set.shrink_undo_registry_prepared(&mut spare, &mut retired),
        0
    );
    assert_eq!(set.undo_groups.capacity(), 0);
    assert_eq!(retired.capacity(), 4);
}

#[test]
fn indexed_registry_removal_preserves_moved_groups_and_allows_reregistration() {
    let mut registry = SemUndoRegistry::default();
    registry.prepare(16).unwrap();
    let mut spare = SemUndoRegistry::default();
    let owners: Vec<_> = (0..6)
        .map(|_| SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap())
        .collect();
    for owner in &owners {
        registry.register_prepared(owner, &mut spare).unwrap();
    }
    // First removal moves the tail into slot zero; then remove a middle slot
    // and the current tail. Duplicate/missing operations must remain harmless.
    for index in [0, 2, 3] {
        registry.unregister(&owners[index]);
        registry.unregister(&owners[index]);
    }
    assert_eq!(registry.len(), 3);
    for index in [1, 4, 5] {
        registry
            .register_prepared(&owners[index], &mut spare)
            .unwrap();
        assert_eq!(registry.len(), 3);
        assert!(registry
            .iter()
            .any(|entry| { entry.group().ptr_eq(&Arc::downgrade(&owners[index])) }));
    }
    for index in [0, 2, 3] {
        registry
            .register_prepared(&owners[index], &mut spare)
            .unwrap();
    }
    assert_eq!(registry.len(), owners.len());
    for owner in &owners {
        registry.unregister(owner);
    }
    assert!(registry.is_empty());
}

#[test]
fn prepared_registry_growth_rechecks_registration_and_capacity() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(151), &[1]);
    let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    let mut spare = SemUndoRegistry::default();
    let capacity = manager
        .get_by_semid_checked_mut(semid)
        .unwrap()
        .ensure_undo_group_registered_prepared(&group, &mut spare)
        .unwrap_err();
    assert!(!manager.undo_registry_contains_for_test(&group));
    assert!(manager
        .get_by_semid_checked(semid)
        .unwrap()
        .undo_groups
        .is_empty());
    spare.prepare(capacity).unwrap();

    // Simulate other first-time groups consuming the prepared capacity
    // while this caller has dropped the manager lock.
    let mut owners = Vec::new();
    while manager
        .get_by_semid_checked(semid)
        .unwrap()
        .undo_groups
        .len()
        < spare.capacity()
    {
        let owner = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        manager.ensure_undo_group_registered(&owner, semid).unwrap();
        owners.push(owner);
    }
    let capacity = manager
        .get_by_semid_checked_mut(semid)
        .unwrap()
        .ensure_undo_group_registered_prepared(&group, &mut spare)
        .unwrap_err();
    assert!(!manager.undo_registry_contains_for_test(&group));
    spare.prepare(capacity).unwrap();
    manager
        .get_by_semid_checked_mut(semid)
        .unwrap()
        .ensure_undo_group_registered_prepared(&group, &mut spare)
        .unwrap();
    assert!(spare.is_empty());
    assert!(manager.undo_registry_contains_for_test(&group));
    assert_eq!(
        manager
            .get_by_semid_checked(semid)
            .unwrap()
            .undo_groups
            .len(),
        owners.len() + 1
    );
    for (weak, owner) in manager
        .get_by_semid_checked(semid)
        .unwrap()
        .undo_groups
        .iter()
        .zip(&owners)
    {
        assert!(weak.group().ptr_eq(&Arc::downgrade(owner)));
    }

    // A concurrent CLONE_SYSVSEM sharer already registered this group.
    let mut empty_spare = SemUndoRegistry::default();
    manager
        .get_by_semid_checked_mut(semid)
        .unwrap()
        .ensure_undo_group_registered_prepared(&group, &mut empty_spare)
        .unwrap();
    assert_eq!(
        manager
            .get_by_semid_checked(semid)
            .unwrap()
            .undo_groups
            .len(),
        owners.len() + 1
    );
}

#[test]
fn queued_undo_commits_to_captured_group_not_waker_current_task() {
    let ipc_ns = test_ipc_ns();
    let group_a = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
    let group_b = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
    let mut manager = ipc_ns.sem.lock();
    let semid = insert_test_set(&mut manager, SemKey::new(46), &[0]);
    manager
        .prepare_undo_record_and_registry_for_test(&group_a, semid)
        .unwrap();
    manager
        .prepare_undo_record_and_registry_for_test(&group_b, semid)
        .unwrap();

    let (_waiter, waker) = Waiter::new_pair();
    let entry = Arc::new(SemQueueEntry::new_prepared(
        SemQueueEntry::prepare_sops(&[undo_sop(0, -1)]).unwrap(),
        None,
        Some(group_a.clone()),
        Some(group_a.prepare_record_for_test(semid, 1).unwrap()),
        waker,
        SemopScratch::try_new(&[plain_sop(0, 0); 1]).unwrap(),
        SemBlockedOp {
            semnum: 0,
            wait_type: SemWaitType::Increase,
            nowait: false,
        },
    ));
    let set = manager.get_by_semid_checked_mut(semid).unwrap();
    set.enqueue_waiter(entry.clone());
    set.sems[0].val = 1;

    manager.update_queue_for_test(semid);

    assert_eq!(entry.completed_result(), Some(Ok(0)));
    assert_eq!(sem_values(&manager, semid), vec![0]);
    assert_eq!(group_a.adjustment_for_test(semid, 0), 1);
    assert_eq!(group_b.adjustment_for_test(semid, 0), 0);
}

#[test]
fn queued_timeout_signal_and_rmid_never_commit_adjustment() {
    let ipc_ns = test_ipc_ns();
    let group = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
    let mut manager = ipc_ns.sem.lock();
    let timeout_semid = insert_test_set(&mut manager, SemKey::new(47), &[0]);
    let signal_semid = insert_test_set(&mut manager, SemKey::new(48), &[0]);
    let rmid_semid = insert_test_set(&mut manager, SemKey::new(49), &[0]);
    for semid in [timeout_semid, signal_semid, rmid_semid] {
        manager
            .prepare_undo_record_and_registry_for_test(&group, semid)
            .unwrap();
    }

    let timeout_entry = enqueue_undo_waiter_for_test(&mut manager, timeout_semid, &group);
    assert_eq!(
        manager.cancel_queued_entry(
            timeout_semid,
            &timeout_entry,
            SystemError::EAGAIN_OR_EWOULDBLOCK,
        ),
        Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
    );
    assert_eq!(group.adjustment_for_test(timeout_semid, 0), 0);

    let signal_entry = enqueue_undo_waiter_for_test(&mut manager, signal_semid, &group);
    assert_eq!(
        manager.cancel_queued_entry(signal_semid, &signal_entry, SystemError::EINTR),
        Err(SystemError::EINTR)
    );
    assert_eq!(group.adjustment_for_test(signal_semid, 0), 0);

    let rmid_entry = enqueue_undo_waiter_for_test(&mut manager, rmid_semid, &group);
    let mut wakes = SemWakeBatch::default();
    let removed = manager.ipc_rmid(rmid_semid, &mut wakes).unwrap();
    drop(manager);
    wakes.wake_all();
    drop(removed);
    assert_eq!(rmid_entry.completed_result(), Some(Err(SystemError::EIDRM)));
    assert_eq!(group.adjustment_for_test(rmid_semid, 0), 0);
}

#[test]
fn first_record_is_registry_visible_before_future_cleanup() {
    let ipc_ns = test_ipc_ns();
    let group = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
    let mut manager = ipc_ns.sem.lock();
    let semid = insert_test_set(&mut manager, SemKey::new(50), &[3]);

    manager
        .prepare_undo_record_and_registry_for_test(&group, semid)
        .unwrap();

    assert_eq!(manager.live_undo_group_count_for_test(), 1);
    assert!(manager.undo_registry_contains_for_test(&group));
    assert_eq!(group.adjustment_for_test(semid, 0), 0);
}

#[test]
fn stale_weak_entries_are_compacted_without_losing_live_group() {
    let ipc_ns = test_ipc_ns();
    let live = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
    let candidate = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
    let stale = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
    let mut manager = ipc_ns.sem.lock();
    let semid = insert_test_set(&mut manager, SemKey::new(51), &[1]);
    manager.ensure_undo_group_registered(&stale, semid).unwrap();
    manager.ensure_undo_group_registered(&live, semid).unwrap();
    drop(stale);
    manager
        .get_by_semid_checked_mut(semid)
        .unwrap()
        .undo_groups
        .compact();

    manager
        .prepare_undo_record_and_registry_for_test(&candidate, semid)
        .unwrap();

    assert_eq!(manager.live_undo_group_count_for_test(), 2);
    assert!(manager.undo_registry_contains_for_test(&live));
    assert!(manager.undo_registry_contains_for_test(&candidate));
}

#[test]
fn live_group_registration_survives_debt_removal_without_duplicates() {
    let ipc_ns = test_ipc_ns();
    let group = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
    let mut manager = ipc_ns.sem.lock();
    let semid = insert_test_set(&mut manager, SemKey::new(150), &[1]);
    manager
        .prepare_undo_record_and_registry_for_test(&group, semid)
        .unwrap();
    let capacity = manager
        .get_by_semid_checked(semid)
        .unwrap()
        .undo_groups
        .capacity();
    manager.clear_undo_for_setall(semid);
    // Synthetic record removal exercises registration deduplication without
    // putting a live set into the RMID-only retired association state.
    group.remove_record(semid);
    assert_eq!(group.record_count_for_test(), 0);
    for _ in 0..32 {
        manager.ensure_undo_group_registered(&group, semid).unwrap();
    }
    assert!(manager.undo_registry_contains_for_test(&group));
    assert_eq!(
        manager
            .get_by_semid_checked(semid)
            .unwrap()
            .undo_groups
            .len(),
        1
    );
    assert_eq!(
        manager
            .get_by_semid_checked(semid)
            .unwrap()
            .undo_groups
            .capacity(),
        capacity
    );
    manager
        .prepare_undo_record_and_registry_for_test(&group, semid)
        .unwrap();
    assert_eq!(
        manager
            .get_by_semid_checked(semid)
            .unwrap()
            .undo_groups
            .len(),
        1
    );
}

#[test]
fn queued_undo_entry_retains_group_after_external_owner_drops() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(52), &[1]);
    let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    manager
        .prepare_undo_record_and_registry_for_test(&group, semid)
        .unwrap();
    let entry = enqueue_undo_waiter_for_test(&mut manager, semid, &group);
    let group_weak = Arc::downgrade(&group);
    drop(group);
    assert!(group_weak.upgrade().is_some());
    manager.get_by_semid_checked_mut(semid).unwrap().sems[0].val = 1;

    manager.update_queue_for_test(semid);

    assert_eq!(entry.completed_result(), Some(Ok(0)));
    assert_eq!(sem_values(&manager, semid), vec![0]);
}

#[test]
fn queued_undo_record_length_mismatch_completes_with_internal_error() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(53), &[1, 1]);
    let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    group.insert_test_record(semid, &[0]);
    let entry = enqueue_undo_waiter_for_test(&mut manager, semid, &group);
    manager.get_by_semid_checked_mut(semid).unwrap().sems[0].val = 1;

    manager.update_queue_for_test(semid);

    assert_eq!(entry.completed_result(), Some(Err(SystemError::EINVAL)));
    assert_eq!(sem_values(&manager, semid), vec![1, 1]);
    assert_eq!(group.adjustment_for_test(semid, 0), 0);
}

#[test]
fn final_owner_detach_replays_against_setval_as_a_single_serial_order() {
    let ipc_ns = test_ipc_ns();
    let semid = {
        let mut manager = ipc_ns.sem.lock();
        insert_test_set(&mut manager, SemKey::new(62), &[2])
    };
    let (pcb, group) = test_pcb_with_group(&ipc_ns);
    group.insert_test_record(semid, &[3]);
    ipc_ns
        .sem
        .lock()
        .ensure_undo_group_registered(&group, semid)
        .unwrap();
    drop(pcb.take_sem_undo_attachment().unwrap());
    assert!(group.detach_last_owner_for_test());

    {
        let mut manager = ipc_ns.sem.lock();
        manager
            .setval(semid, 0, 7, &mut SemWakeBatch::default())
            .unwrap();
    }
    group.replay_marked_records_for_test(&pcb);

    let manager = ipc_ns.sem.lock();
    assert_eq!(sem_values(&manager, semid), vec![7]);
    assert_eq!(group.record_count_for_test(), 0);
    assert_eq!(group.replay_count_for_test(), 1);
    assert!(pcb.sem_undo_group().is_none());
}

#[test]
fn rmid_before_final_owner_replay_skips_detached_record_once() {
    let ipc_ns = test_ipc_ns();
    let semid = {
        let mut manager = ipc_ns.sem.lock();
        insert_test_set(&mut manager, SemKey::new(63), &[2])
    };
    let (pcb, group) = test_pcb_with_group(&ipc_ns);
    group.insert_test_record(semid, &[3]);
    ipc_ns
        .sem
        .lock()
        .ensure_undo_group_registered(&group, semid)
        .unwrap();
    drop(pcb.take_sem_undo_attachment().unwrap());
    assert!(group.detach_last_owner_for_test());

    let mut wakes = SemWakeBatch::default();
    let removed = {
        let mut manager = ipc_ns.sem.lock();
        manager.ipc_rmid(semid, &mut wakes).unwrap()
    };
    wakes.wake_all();
    drop(removed);
    group.replay_marked_records_for_test(&pcb);

    let manager = ipc_ns.sem.lock();
    assert!(matches!(
        manager.get_by_semid_checked(semid),
        Err(SystemError::EINVAL)
    ));
    assert_eq!(group.record_count_for_test(), 0);
    assert_eq!(group.replay_count_for_test(), 1);
    assert!(pcb.sem_undo_group().is_none());
}

#[test]
fn prepare_existing_undo_record_length_mismatch_returns_einval_without_mutation() {
    let semid = SemId::new(64);
    let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    group.insert_test_record(semid, &[5]);

    let err = group.prepare_record_for_test(semid, 2).unwrap_err();

    assert_eq!(err, SystemError::EINVAL);
    assert_eq!(group.adjustment_for_test(semid, 0), 5);
    assert_eq!(group.record_count_for_test(), 1);
    assert_eq!(group.pending_record_reservations_for_test(), 0);
}

#[test]
fn setval_clears_only_target_sem_adjustment_across_all_groups() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(54), &[4, 5]);
    let other_semid = insert_test_set(&mut manager, SemKey::new(55), &[6, 7]);
    let group_a = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    let group_b = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    group_a.insert_test_record(semid, &[11, 12]);
    group_b.insert_test_record(semid, &[21, 22]);
    group_a.insert_test_record(other_semid, &[31, 32]);
    manager
        .ensure_undo_group_registered(&group_a, semid)
        .unwrap();
    manager
        .ensure_undo_group_registered(&group_b, semid)
        .unwrap();

    manager
        .setval(semid, 0, 9, &mut SemWakeBatch::default())
        .unwrap();

    assert_eq!(sem_values(&manager, semid), vec![9, 5]);
    assert_eq!(group_a.adjustment_for_test(semid, 0), 0);
    assert_eq!(group_a.adjustment_for_test(semid, 1), 12);
    assert_eq!(group_b.adjustment_for_test(semid, 0), 0);
    assert_eq!(group_b.adjustment_for_test(semid, 1), 22);
    assert_eq!(group_a.adjustment_for_test(other_semid, 0), 31);
}

#[test]
fn setall_clears_entire_full_semid_record_across_all_groups() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(56), &[1, 2, 3]);
    let other_semid = insert_test_set(&mut manager, SemKey::new(57), &[4, 5, 6]);
    let group_a = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    let group_b = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    group_a.insert_test_record(semid, &[1, 2, 3]);
    group_b.insert_test_record(semid, &[4, 5, 6]);
    group_b.insert_test_record(other_semid, &[7, 8, 9]);
    manager
        .ensure_undo_group_registered(&group_a, semid)
        .unwrap();
    manager
        .ensure_undo_group_registered(&group_b, semid)
        .unwrap();
    let token = manager.prepare_setall(semid).unwrap();

    manager
        .setall(token, &[10, 11, 12], &mut SemWakeBatch::default())
        .unwrap();

    assert_eq!(sem_values(&manager, semid), vec![10, 11, 12]);
    assert_eq!(group_a.adjustment_for_test(semid, 0), 0);
    assert_eq!(group_a.adjustment_for_test(semid, 1), 0);
    assert_eq!(group_a.adjustment_for_test(semid, 2), 0);
    assert_eq!(group_b.adjustment_for_test(semid, 0), 0);
    assert_eq!(group_b.adjustment_for_test(semid, 1), 0);
    assert_eq!(group_b.adjustment_for_test(semid, 2), 0);
    assert_eq!(group_b.adjustment_for_test(other_semid, 1), 8);
}

#[test]
fn setval_cleanup_precedes_value_write_and_queue_rescan() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(58), &[0]);
    let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    group.insert_test_record(semid, &[7]);
    manager.ensure_undo_group_registered(&group, semid).unwrap();
    let entry = enqueue_undo_waiter_for_test(&mut manager, semid, &group);

    manager
        .setval(semid, 0, 1, &mut SemWakeBatch::default())
        .unwrap();

    assert_eq!(entry.completed_result(), Some(Ok(0)));
    assert_eq!(sem_values(&manager, semid), vec![0]);
    assert_eq!(group.adjustment_for_test(semid, 0), 1);
}

#[test]
fn rmid_discards_record_before_index_can_be_reused() {
    let mut manager = SemManager::new();
    manager.reset_allocator_for_test(2);
    let old_semid = insert_test_set(&mut manager, SemKey::new(59), &[3]);
    let filler_semid = insert_test_set(&mut manager, SemKey::new(60), &[4]);
    let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    group.insert_test_record(old_semid, &[9]);
    manager
        .ensure_undo_group_registered(&group, old_semid)
        .unwrap();
    manager
        .ipc_rmid(old_semid, &mut SemWakeBatch::default())
        .unwrap();

    let new_semid = insert_test_set(&mut manager, SemKey::new(61), &[5]);

    assert_ne!(old_semid, new_semid);
    assert_eq!(
        old_semid.data() & IpcIdAllocator::IPC_ID_IDX_MASK,
        new_semid.data() & IpcIdAllocator::IPC_ID_IDX_MASK
    );
    assert_eq!(sem_values(&manager, new_semid), vec![5]);
    assert_eq!(group.record_count_for_test(), 0);
    assert_eq!(sem_values(&manager, filler_semid), vec![4]);
}

#[test]
fn queued_stale_existing_record_retries_and_completes_without_error() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(66), &[0]);
    let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    group.insert_test_record(semid, &[7]);
    manager.ensure_undo_group_registered(&group, semid).unwrap();
    let entry = enqueue_undo_waiter_for_test(&mut manager, semid, &group);

    manager.clear_undo_for_setval(semid, 0);
    manager.get_by_semid_checked_mut(semid).unwrap().sems[0].val = 2;
    manager.update_queue_for_test(semid);

    assert_eq!(entry.completed_result(), Some(Ok(0)));
    assert_eq!(sem_values(&manager, semid), vec![1]);
    assert_eq!(group.adjustment_for_test(semid, 0), 1);
}

#[test]
fn const_waiters_complete_before_altering_waiters() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(67), &[1]);
    let (altering, constant) = {
        let set = manager.get_by_semid_checked_mut(semid).unwrap();
        let altering = enqueue_test_waiter(
            set,
            &[plain_sop(0, 0), plain_sop(0, 1)],
            SemBlockedOp {
                semnum: 0,
                wait_type: SemWaitType::Zero,
                nowait: false,
            },
        );
        let constant = enqueue_test_waiter(
            set,
            &[plain_sop(0, 0)],
            SemBlockedOp {
                semnum: 0,
                wait_type: SemWaitType::Zero,
                nowait: false,
            },
        );
        set.sems[0].val = 0;
        (altering, constant)
    };

    manager.update_queue_for_test(semid);

    let set = manager.get_by_semid_checked(semid).unwrap();
    assert_eq!(constant.completed_result(), Some(Ok(0)));
    assert_eq!(altering.completed_result(), Some(Ok(0)));
    assert!(set.pending_is_empty());
    assert_eq!(set.sems[0].val, 1);
}
