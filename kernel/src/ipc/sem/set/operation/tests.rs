use super::*;

#[test]
fn try_apply_commits_metadata_even_when_values_are_unchanged() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(301), &[4]);
    let set = manager.get_by_semid_checked_mut(semid).unwrap();
    set.sem_otime = -1;
    let mut scratch = SemopScratch::try_new(&[plain_sop(0, 0); 2]).unwrap();
    assert!(matches!(
        set.try_apply(
            &[plain_sop(0, -1), plain_sop(0, 1)],
            None,
            None,
            &mut scratch
        ),
        Ok(SemAttempt::Completed {
            waiter_state_changed: false
        })
    ));
    assert_eq!(set.sems[0].val, 4);
    assert_ne!(set.sem_otime, -1);
}

#[test]
fn try_apply_rebuilds_scratch_after_a_blocked_attempt() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(302), &[0]);
    let set = manager.get_by_semid_checked_mut(semid).unwrap();
    set.sem_otime = -1;
    let mut scratch = SemopScratch::try_new(&[plain_sop(0, 0); 1]).unwrap();
    assert!(matches!(
        set.try_apply(&[plain_sop(0, -1)], None, None, &mut scratch),
        Ok(SemAttempt::Blocked(_))
    ));
    assert_eq!(set.sem_otime, -1);
    set.sems[0].val = 1;
    assert!(matches!(
        set.try_apply(&[plain_sop(0, -1)], None, None, &mut scratch),
        Ok(SemAttempt::Completed {
            waiter_state_changed: true
        })
    ));
    assert_eq!(set.sems[0].val, 0);
}
use crate::ipc::sem::manager::SemManager;
use crate::ipc::sem::set::test_support::*;
use crate::process::namespace::ipc_namespace::INIT_IPC_NAMESPACE;

#[test]
fn setval_clear_between_prepare_and_commit_refreshes_stale_existing_record() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(62), &[4]);
    let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    group.insert_test_record(semid, &[7]);
    manager.ensure_undo_group_registered(&group, semid).unwrap();

    let record = group.prepare_record_for_test(semid, 1).unwrap();
    manager.clear_undo_for_setval(semid, 0);
    let mut scratch = SemopScratch::try_new(&[plain_sop(0, 0); 1]).unwrap();
    let set = manager.get_by_semid_checked_mut(semid).unwrap();

    let result = group.with_prepared_record_noalloc(record, |record| {
        let outcome =
            KernelSemSet::simulate_semop(set, &[undo_sop(0, -1)], Some(record), &mut scratch)
                .unwrap()
                .ready_for_test();
        KernelSemSet::commit_semop(set, outcome, &scratch, None, Some(record));
        PreparedSemUndoRecordAction::Complete(())
    });

    assert!(result.is_ok());
    assert_eq!(sem_values(&manager, semid), vec![3]);
    assert_eq!(group.adjustment_for_test(semid, 0), 1);
}

#[test]
fn setall_clear_between_prepare_and_commit_refreshes_stale_existing_record() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(63), &[4, 5]);
    let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    group.insert_test_record(semid, &[7, -3]);
    manager.ensure_undo_group_registered(&group, semid).unwrap();

    let record = group.prepare_record_for_test(semid, 2).unwrap();
    manager.clear_undo_for_setall(semid);
    let mut scratch = SemopScratch::try_new(&[plain_sop(0, 0); 1]).unwrap();
    let set = manager.get_by_semid_checked_mut(semid).unwrap();

    let result = group.with_prepared_record_noalloc(record, |record| {
        let outcome =
            KernelSemSet::simulate_semop(set, &[undo_sop(0, -1)], Some(record), &mut scratch)
                .unwrap()
                .ready_for_test();
        KernelSemSet::commit_semop(set, outcome, &scratch, None, Some(record));
        PreparedSemUndoRecordAction::Complete(())
    });

    assert!(result.is_ok());
    assert_eq!(sem_values(&manager, semid), vec![3, 5]);
    assert_eq!(group.adjustment_for_test(semid, 0), 1);
    assert_eq!(group.adjustment_for_test(semid, 1), 0);
}

#[test]
fn stale_existing_prepared_record_refreshes_before_immediate_commit() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(65), &[4]);
    let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    group.insert_test_record(semid, &[7]);
    manager.ensure_undo_group_registered(&group, semid).unwrap();

    let record = group.prepare_record_for_test(semid, 1).unwrap();
    manager.clear_undo_for_setval(semid, 0);
    let mut scratch = SemopScratch::try_new(&[plain_sop(0, 0); 1]).unwrap();
    let set = manager.get_by_semid_checked_mut(semid).unwrap();

    let result = group.with_prepared_record_noalloc(record, |record| {
        let outcome =
            KernelSemSet::simulate_semop(set, &[undo_sop(0, -1)], Some(record), &mut scratch)
                .unwrap()
                .ready_for_test();
        KernelSemSet::commit_semop(set, outcome, &scratch, None, Some(record));
        PreparedSemUndoRecordAction::Complete(())
    });

    assert!(result.is_ok());
    assert_eq!(sem_values(&manager, semid), vec![3]);
    assert_eq!(group.adjustment_for_test(semid, 0), 1);
}

#[test]
fn consecutive_sem_undo_on_unchanged_existing_record_still_accumulates() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(64), &[5]);
    let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    group.insert_test_record(semid, &[2]);

    let record = group.prepare_record_for_test(semid, 1).unwrap();
    let mut scratch = SemopScratch::try_new(&[plain_sop(0, 0); 1]).unwrap();
    let set = manager.get_by_semid_checked_mut(semid).unwrap();
    group
        .with_prepared_record_noalloc(record, |record| {
            let outcome =
                KernelSemSet::simulate_semop(set, &[undo_sop(0, -2)], Some(record), &mut scratch)
                    .unwrap()
                    .ready_for_test();
            KernelSemSet::commit_semop(set, outcome, &scratch, None, Some(record));
            PreparedSemUndoRecordAction::Complete(())
        })
        .unwrap();

    assert_eq!(group.adjustment_for_test(semid, 0), 4);
}

#[test]
fn ordered_mixed_undo_ops_apply_each_adjustment_step() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(41), &[4]);
    let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    let record = group.prepare_record_for_test(semid, 1).unwrap();
    let mut scratch = SemopScratch::try_new(&[plain_sop(0, 0); 3]).unwrap();
    let sops = [undo_sop(0, 3), plain_sop(0, -1), undo_sop(0, -2)];

    let set = manager.get_by_semid_checked_mut(semid).unwrap();
    group
        .with_prepared_record_noalloc(record, |record| {
            let outcome = KernelSemSet::simulate_semop(set, &sops, Some(record), &mut scratch);
            assert!(matches!(outcome, Ok(SemopOutcome::Ready(_))));
            KernelSemSet::commit_semop(
                set,
                outcome.unwrap().ready_for_test(),
                &scratch,
                None,
                Some(record),
            );
            PreparedSemUndoRecordAction::Complete(())
        })
        .unwrap();

    assert_eq!(set.sems[0].val, 4);
    assert_eq!(group.adjustment_for_test(semid, 0), -1);
}

#[test]
fn intermediate_adjustment_overflow_is_erange_even_if_later_op_cancels_it() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(42), &[10]);
    let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    group.insert_test_record(semid, &[i16::MAX]);
    let record = group.prepare_record_for_test(semid, 1).unwrap();
    let mut scratch = SemopScratch::try_new(&[plain_sop(0, 0); 2]).unwrap();
    let sops = [undo_sop(0, -1), undo_sop(0, 1)];

    let set = manager.get_by_semid_checked_mut(semid).unwrap();
    group
        .with_prepared_record_noalloc(record, |record| {
            assert!(matches!(
                KernelSemSet::simulate_semop(set, &sops, Some(record), &mut scratch),
                Err(SystemError::ERANGE)
            ));
            PreparedSemUndoRecordAction::Keep(())
        })
        .unwrap();

    assert_eq!(set.sems[0].val, 10);
    assert_eq!(group.adjustment_for_test(semid, 0), i16::MAX);
}

#[test]
fn blocked_or_nowait_failure_does_not_commit_semval_or_semadj_prefix() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(43), &[2]);
    let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    // Exercise the live record, not just an unpublished candidate.
    group.insert_test_record(semid, &[0]);
    let record = group.prepare_record_for_test(semid, 1).unwrap();
    let mut scratch = SemopScratch::try_new(&[plain_sop(0, 0); 2]).unwrap();
    let sops = [undo_sop(0, -1), nowait_sop(0, -2)];

    let set = manager.get_by_semid_checked_mut(semid).unwrap();
    group
        .with_prepared_record_noalloc(record, |record| {
            assert!(matches!(
                KernelSemSet::simulate_semop(set, &sops, Some(record), &mut scratch),
                Ok(SemopOutcome::Blocked(SemBlockedOp {
                    semnum: 0,
                    wait_type: SemWaitType::Increase,
                    nowait: true,
                }))
            ));
            PreparedSemUndoRecordAction::Keep(())
        })
        .unwrap();

    assert_eq!(set.sems[0].val, 2);
    assert_eq!(group.adjustment_for_test(semid, 0), 0);
}

#[test]
fn zero_undo_op_can_prepare_zero_record_without_adjustment() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(44), &[0]);
    let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
    let record = group.prepare_record_for_test(semid, 1).unwrap();
    let mut scratch = SemopScratch::try_new(&[plain_sop(0, 0); 1]).unwrap();
    let sops = [undo_sop(0, 0)];

    let set = manager.get_by_semid_checked_mut(semid).unwrap();
    group
        .with_prepared_record_noalloc(record, |record| {
            let outcome = KernelSemSet::simulate_semop(set, &sops, Some(record), &mut scratch);
            assert!(matches!(outcome, Ok(SemopOutcome::Ready(_))));
            KernelSemSet::commit_semop(
                set,
                outcome.unwrap().ready_for_test(),
                &scratch,
                None,
                Some(record),
            );
            PreparedSemUndoRecordAction::Complete(())
        })
        .unwrap();

    assert_eq!(set.sems[0].val, 0);
    assert_eq!(group.record_count_for_test(), 1);
}

#[test]
fn scratch_rejects_an_incompatible_operation_array() {
    let mut manager = SemManager::new();
    let semid = insert_test_set(&mut manager, SemKey::new(45), &[1, 1]);
    let set = manager.get_by_semid_checked_mut(semid).unwrap();
    let mut scratch = SemopScratch::try_new(&[plain_sop(0, 0); 1]).unwrap();
    let sops = [plain_sop(0, -1), plain_sop(1, -1)];

    assert!(matches!(
        KernelSemSet::simulate_semop(set, &sops, None, &mut scratch),
        Err(SystemError::EINVAL)
    ));
}
