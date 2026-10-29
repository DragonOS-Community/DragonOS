use alloc::sync::Arc;

use super::{
    detach_sem_undo, PreparedSemUndoRecord, PreparedSemUndoRecordAction, SemUndoAttachment,
    SemUndoGroup, SemUndoRecord,
};
use crate::ipc::sem::SemId;
use crate::process::{
    fork::CloneFlags,
    namespace::ipc_namespace::{IpcNamespace, INIT_IPC_NAMESPACE},
    KernelStack, ProcessControlBlock,
};
use system_error::SystemError;

fn test_ipc_ns() -> &'static Arc<IpcNamespace> {
    &INIT_IPC_NAMESPACE
}

fn test_unpublished_child() -> Arc<ProcessControlBlock> {
    ProcessControlBlock::new_idle(0, KernelStack::new().unwrap())
}

fn test_pcb_with_group() -> Arc<ProcessControlBlock> {
    let pcb = test_unpublished_child();
    pcb.ensure_sem_undo_group(test_ipc_ns()).unwrap();
    pcb
}

fn second_test_ipc_ns() -> Arc<IpcNamespace> {
    INIT_IPC_NAMESPACE.copy_ipc_ns(
        &CloneFlags::CLONE_NEWIPC,
        INIT_IPC_NAMESPACE.user_ns.clone(),
    )
}

#[test]
fn missing_reservation_cannot_publish_after_final_drain() {
    let pcb = test_pcb_with_group();
    let group = pcb.sem_undo_group().unwrap();
    let semid = SemId::new(208);
    let record = group.prepare_record_for_test(semid, 1).unwrap();

    detach_sem_undo(&pcb);

    assert_eq!(group.task_owners_for_test(), 0);
    assert_eq!(group.commit_record(record), Err(SystemError::EINVAL));
    assert_eq!(group.record_count_for_test(), 0);
    assert_eq!(group.pending_record_reservations_for_test(), 0);
    assert!(matches!(
        group.prepare_record_for_test(semid, 1),
        Err(SystemError::EINVAL)
    ));
}

#[test]
fn retired_pending_debt_stays_visible_between_replay_steps() {
    use crate::ipc::sem::{SemFlags, SemWakeBatch, IPC_PRIVATE};
    for control in 0..3 {
        let ipc_ns = second_test_ipc_ns();
        let group = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
        let (pending, first) = {
            let mut manager = ipc_ns.sem.lock();
            let pending = SemId::new(
                manager
                    .semget_for_test(IPC_PRIVATE, 2, SemFlags::IPC_CREAT)
                    .unwrap(),
            );
            let first = SemId::new(
                manager
                    .semget_for_test(IPC_PRIVATE, 1, SemFlags::IPC_CREAT)
                    .unwrap(),
            );
            manager
                .prepare_undo_record_and_registry_for_test(&group, pending)
                .unwrap();
            manager
                .prepare_undo_record_and_registry_for_test(&group, first)
                .unwrap();
            group.with_record_mut(pending, |record| {
                record.adjustments.copy_from_slice(&[7, -3])
            });
            group.with_record_mut(first, |record| record.adjustments[0] = 1);
            (pending, first)
        };
        assert!(group.detach_owner_and_mark_last());
        assert!(group.begin_replay());
        assert!(!group.begin_replay());
        // Vec.pop chooses the last inserted set. The other debt must
        // remain in the same registry visited by the semctl primitives.
        {
            let mut wakes = SemWakeBatch::default();
            let record = {
                let mut manager = ipc_ns.sem.lock();
                let record = group.pop_retired_record().unwrap();
                assert_eq!(record.semid, first);
                manager.replay_sem_undo_adjustments(
                    record.semid,
                    &record.adjustments,
                    None,
                    &mut wakes,
                );
                manager.unregister_undo_group(record.semid, &group);
                record
            };
            wakes.wake_all();
            drop(record);
        }
        assert_eq!(group.record_count_for_test(), 1);
        let mut wakes = SemWakeBatch::default();
        let mut removed = None;
        let mut manager = ipc_ns.sem.lock();
        match control {
            0 => manager.setval(pending, 0, 9, &mut wakes).unwrap(),
            1 => {
                let token = manager.prepare_setall(pending).unwrap();
                manager.setall(token, &[9, 8], &mut wakes).unwrap();
            }
            _ => removed = Some(manager.ipc_rmid(pending, &mut wakes).unwrap()),
        }
        let next = group.pop_retired_record();
        if control == 2 {
            assert!(next.is_none());
        } else {
            let next = next.as_ref().unwrap();
            assert_eq!(next.adjustment(0), 0);
            assert_eq!(next.adjustment(1), if control == 0 { -3 } else { 0 });
            manager.replay_sem_undo_adjustments(next.semid, &next.adjustments, None, &mut wakes);
            manager.unregister_undo_group(next.semid, &group);
            assert_eq!(
                manager.getall(pending).unwrap(),
                if control == 0 { vec![9, 0] } else { vec![9, 8] }
            );
        }
        assert!(group.pop_retired_record().is_none());
        assert!(!group.begin_replay());
        drop(manager);
        wakes.wake_all();
        drop(removed);
        drop(next);
    }
}

#[test]
fn retired_group_rejects_shared_owner_and_replays_only_once() {
    let pcb = test_pcb_with_group();
    let group = pcb.sem_undo_group().unwrap();
    let child = test_unpublished_child();
    group.insert_test_record(SemId::new(209), &[1]);

    detach_sem_undo(&pcb);

    assert_eq!(group.replay_count_for_test(), 1);
    assert_eq!(group.task_owners_for_test(), 0);
    assert!(matches!(
        pcb.prepare_shared_sem_undo_attachment(test_ipc_ns()),
        Err(SystemError::EINVAL)
    ));
    group.replay_marked_records_for_test(&pcb);
    detach_sem_undo(&child);
    assert_eq!(group.replay_count_for_test(), 1);
}

#[test]
fn prepared_existing_and_missing_records_cannot_publish_after_final_drain() {
    let pcb = test_pcb_with_group();
    let group = pcb.sem_undo_group().unwrap();
    let existing_semid = SemId::new(210);
    let missing_semid = SemId::new(211);
    group.insert_test_record(existing_semid, &[1]);
    let existing = group.prepare_record_for_test(existing_semid, 1).unwrap();
    let missing = group.prepare_record_for_test(missing_semid, 1).unwrap();

    detach_sem_undo(&pcb);

    assert_eq!(group.commit_record(existing), Err(SystemError::EINVAL));
    assert_eq!(group.commit_record(missing), Err(SystemError::EINVAL));
    assert_eq!(group.record_count_for_test(), 0);
    assert_eq!(group.pending_record_reservations_for_test(), 0);
}

#[test]
fn prepare_existing_record_is_rejected_after_final_drain() {
    let pcb = test_pcb_with_group();
    let group = pcb.sem_undo_group().unwrap();
    let semid = SemId::new(210);
    group.insert_test_record(semid, &[1]);

    detach_sem_undo(&pcb);

    assert_eq!(group.record_count_for_test(), 0);
    assert!(matches!(
        group.prepare_record_for_test(semid, 1),
        Err(SystemError::EINVAL)
    ));
}

#[test]
fn namespace_lifecycle_invariant_has_no_live_record_at_final_drop() {
    let ipc_ns = second_test_ipc_ns();
    let group = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
    let mut manager = ipc_ns.sem.lock();
    let semid = manager
        .semget_for_test(
            crate::ipc::sem::IPC_PRIVATE,
            1,
            crate::ipc::sem::SemFlags::IPC_CREAT,
        )
        .unwrap();
    manager
        .prepare_undo_record_and_registry_for_test(&group, SemId::new(semid))
        .unwrap();

    assert!(group.has_live_records_in_namespace_for_test(&ipc_ns));
    drop(manager);
    drop(group);
    assert!(ipc_ns.sem.lock().namespace_lifecycle_invariant_for_test());
}

#[test]
fn prepare_existing_record_borrows_current_adjustments() {
    let group = SemUndoGroup::new_for_test();
    let semid = SemId::new(101);
    group.insert_test_record(semid, &[7, -3]);

    let record = group.prepare_record_for_test(semid, 2).unwrap();

    assert!(record.was_existing());
    group
        .with_prepared_record_noalloc(record, |live| {
            assert_eq!(live.adjustment_for_test(0), 7);
            assert_eq!(live.adjustment_for_test(1), -3);
            PreparedSemUndoRecordAction::Keep(())
        })
        .unwrap();
}

#[test]
fn concurrent_prepare_for_distinct_semids_reserves_each_missing_record_slot() {
    let group = SemUndoGroup::new_for_test();
    let semid_one = SemId::new(201);
    let semid_two = SemId::new(202);

    let record_one = group.prepare_record_for_test(semid_one, 1).unwrap();
    let record_two = group.prepare_record_for_test(semid_two, 1).unwrap();

    assert_eq!(group.pending_record_reservations_for_test(), 2);
    assert!(group.record_capacity_for_test() >= 2);
    group.commit_record(record_one).unwrap();
    group.commit_record(record_two).unwrap();
    assert_eq!(group.record_count_for_test(), 2);
    assert_eq!(group.pending_record_reservations_for_test(), 0);
}

#[test]
fn prepare_missing_record_reserves_capacity_for_two_outstanding_reservations() {
    let group = SemUndoGroup::new_for_test();
    group.set_record_capacity_for_test(1);

    let record_one = group
        .prepare_record_for_test(SemId::new(204), 1)
        .expect("first reservation must fit in the single free slot");
    let record_two = group
        .prepare_record_for_test(SemId::new(205), 1)
        .expect("second reservation must grow physical capacity");

    assert_eq!(group.pending_record_reservations_for_test(), 2);
    assert!(group.record_capacity_for_test() >= 2);
    group.commit_record(record_one).unwrap();
    group.commit_record(record_two).unwrap();
    assert_eq!(group.record_count_for_test(), 2);
}

#[test]
fn missing_record_reservation_loses_to_competing_insert_with_retry() {
    let group = SemUndoGroup::new_for_test();
    let semid = SemId::new(206);
    let stale = group.prepare_record_for_test(semid, 1).unwrap();
    group.insert_test_record(semid, &[9]);

    assert_eq!(group.commit_record(stale), Ok(()));
    assert_eq!(group.adjustment_for_test(semid, 0), 9);
    assert_eq!(group.pending_record_reservations_for_test(), 0);
}

#[test]
fn missing_record_reservation_loses_to_rmid_generation_with_retry() {
    let group = SemUndoGroup::new_for_test();
    let semid = SemId::new(207);
    let stale = group.prepare_record_for_test(semid, 1).unwrap();
    group.remove_record(semid);

    assert_eq!(group.commit_record(stale), Ok(()));
    assert_eq!(group.record_count_for_test(), 1);
    assert_eq!(group.adjustment_for_test(semid, 0), 0);
    assert_eq!(group.pending_record_reservations_for_test(), 0);
}

#[test]
fn commit_unreserved_missing_record_returns_enomem_without_allocating() {
    let group = SemUndoGroup::new_for_test();
    let before_capacity = group.record_capacity_for_test();
    let record = PreparedSemUndoRecord {
        semid: SemId::new(203),
        nsems: 1,
        candidate: Some(SemUndoRecord {
            semid: SemId::new(203),
            adjustments: alloc::vec![0].into_boxed_slice(),
        }),
        reservation: None,
    };

    assert_eq!(group.commit_record(record), Err(SystemError::ENOMEM));
    assert_eq!(group.record_count_for_test(), 0);
    assert_eq!(group.record_capacity_for_test(), before_capacity);
}

#[test]
fn failed_first_operation_keeps_zero_record_without_another_reservation() {
    let group = SemUndoGroup::new_for_test();
    let semid = SemId::new(214);
    let prepared = group.prepare_record_for_test(semid, 2).unwrap();
    assert_eq!(group.record_count_for_test(), 0); // Preparation alone is not publication.
    let (result, token) = group
        .with_prepared_record_noalloc(prepared, |record| {
            assert_eq!(record.adjustment(0), 0);
            PreparedSemUndoRecordAction::Keep(Err::<(), _>(SystemError::EAGAIN_OR_EWOULDBLOCK))
        })
        .unwrap();
    assert_eq!(result, Err(SystemError::EAGAIN_OR_EWOULDBLOCK));
    assert!(token.unwrap().was_existing());
    assert_eq!(group.record_count_for_test(), 1);
    assert_eq!(group.pending_record_reservations_for_test(), 0);
    assert_eq!(group.adjustment_for_test(semid, 0), 0);
    assert_eq!(group.adjustment_for_test(semid, 1), 0);
    assert!(group
        .prepare_record_for_test(semid, 2)
        .unwrap()
        .was_existing());
}

#[test]
fn existing_preparation_reads_current_debt_and_updates_only_one_slot() {
    let group = SemUndoGroup::new_for_test();
    let semid = SemId::new(212);
    group.insert_test_record(semid, &[3, 7]);
    let prepared = group.prepare_record_for_test(semid, 2).unwrap();
    assert!(prepared.was_existing());
    assert!(prepared.candidate.is_none());
    group.with_record_mut(semid, |live| live.set_adjustment(0, 9));
    let (_, kept) = group
        .with_prepared_record_noalloc(prepared, |live| {
            assert_eq!(live.adjustment(0), 9);
            PreparedSemUndoRecordAction::Keep(Err::<(), _>(SystemError::ERANGE))
        })
        .unwrap();
    assert_eq!(group.adjustment_for_test(semid, 0), 9);
    assert_eq!(group.adjustment_for_test(semid, 1), 7);
    group.with_record_mut(semid, |live| live.clear_all_adjustments());
    group
        .with_prepared_record_noalloc(kept.unwrap(), |live| {
            assert_eq!(live.adjustment(0), 0);
            live.set_adjustment(1, 4);
            PreparedSemUndoRecordAction::Complete(())
        })
        .unwrap();
    assert_eq!(group.adjustment_for_test(semid, 0), 0);
    assert_eq!(group.adjustment_for_test(semid, 1), 4);
}

#[test]
fn competing_first_publish_releases_candidate_on_keep() {
    let group = SemUndoGroup::new_for_test();
    let semid = SemId::new(213);
    let pending = group.prepare_record_for_test(semid, 2).unwrap();
    assert!(!pending.was_existing());
    group.insert_test_record(semid, &[6, 8]);
    let (_, kept) = group
        .with_prepared_record_noalloc(pending, |live| {
            assert_eq!(live.adjustment(0), 6);
            PreparedSemUndoRecordAction::Keep(())
        })
        .unwrap();
    assert!(kept.unwrap().was_existing());
    assert_eq!(group.pending_record_reservations_for_test(), 0);
    assert_eq!(group.adjustment_for_test(semid, 1), 8);
}

#[test]
fn observer_arc_does_not_change_task_owner_count() {
    let group = SemUndoGroup::new_for_test();
    let attachment = SemUndoAttachment::new_for_test(group.clone());
    let observer = attachment.group_for_test();
    assert_eq!(group.task_owners_for_test(), 1);
    drop(observer);
    assert_eq!(group.task_owners_for_test(), 1);
}

#[test]
fn attachment_is_taken_once_and_drop_never_replays() {
    let attachment = SemUndoAttachment::new_for_test(SemUndoGroup::new_for_test());
    let mut slot = Some(attachment);
    assert!(slot.take().is_some());
    assert!(slot.take().is_none());
}

#[test]
fn group_rejects_different_ipc_namespace() {
    let group = SemUndoGroup::new_for_test_bound_to_first_namespace();
    assert_eq!(
        group.verify_ipc_ns_for_test(second_test_ipc_ns()),
        Err(SystemError::EINVAL)
    );
}

#[test]
fn ordinary_fork_child_starts_without_attachment() {
    let parent = test_pcb_with_group();
    let child = test_unpublished_child();

    assert!(child.sem_undo_group().is_none());
    assert!(parent.sem_undo_group().is_some());
}

#[test]
fn sysvsem_guard_increments_once_then_install_moves_token() {
    let parent = test_pcb_with_group();
    let group = parent.sem_undo_group().unwrap();
    let child = test_unpublished_child();

    let mut guard = parent
        .prepare_shared_sem_undo_attachment(test_ipc_ns())
        .unwrap();
    assert_eq!(group.task_owners_for_test(), 2);
    guard.install_into(&child);
    assert!(child.sem_undo_group().is_some());
    guard.disarm();
    assert_eq!(group.task_owners_for_test(), 2);
}

#[test]
fn installed_guard_rollback_takes_child_slot_and_only_drops_owner() {
    let parent = test_pcb_with_group();
    let group = parent.sem_undo_group().unwrap();
    let child = test_unpublished_child();

    let mut guard = parent
        .prepare_shared_sem_undo_attachment(test_ipc_ns())
        .unwrap();
    guard.install_into(&child);
    drop(guard);

    assert!(child.sem_undo_group().is_none());
    assert_eq!(group.task_owners_for_test(), 1);
    assert_eq!(group.replay_count_for_test(), 0);
}
