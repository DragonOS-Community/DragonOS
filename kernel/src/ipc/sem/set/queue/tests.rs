use super::*;
use crate::ipc::sem::manager::SemManager;
use crate::ipc::sem::set::test_support::*;
use crate::libs::wait_queue::Waiter;

#[test]
fn pending_links_remove_middle_head_tail_and_preserve_fifo() {
    let mut manager = SemManager::new();
    let id = insert_test_set(&mut manager, SemKey::new(153), &[0]);
    let set = manager.get_by_semid_checked_mut(id).unwrap();
    let ops = [PosixSemBuf {
        sem_num: 0,
        sem_op: -1,
        sem_flg: 0,
    }];
    let mut entries = Vec::new();
    for _ in 0..4 {
        entries.push(enqueue_test_waiter(
            set,
            &ops,
            SemBlockedOp {
                semnum: 0,
                wait_type: SemWaitType::Increase,
                nowait: false,
            },
        ));
    }
    set.remove_waiter(&entries[1]);
    set.remove_waiter(&entries[1]); // An already detached node is harmless.
    let remaining: Vec<_> = set.pending_alter.iter().collect();
    assert_eq!(remaining.len(), 3);
    for (actual, expected) in remaining.iter().zip([0, 2, 3]) {
        assert!(Arc::ptr_eq(actual, &entries[expected]));
    }
    set.remove_waiter(&entries[0]);
    set.remove_waiter(&entries[3]);
    assert!(Arc::ptr_eq(
        set.pending_alter.head.as_ref().unwrap(),
        &entries[2]
    ));
    assert!(Arc::ptr_eq(
        set.pending_alter.tail.as_ref().unwrap(),
        &entries[2]
    ));
    set.remove_waiter(&entries[2]);
    assert!(set.pending_is_empty());
    for entry in entries {
        assert!(entry.complete(Err(SystemError::EINTR)));
    }
}

#[test]
fn wait_counts_follow_only_linked_current_blockers() {
    let mut manager = SemManager::new();
    let id = insert_test_set(&mut manager, SemKey::new(156), &[0, 1]);
    let set = manager.get_by_semid_checked_mut(id).unwrap();
    let (_waiter, waker) = Waiter::new_pair();
    let increase = SemBlockedOp {
        semnum: 0,
        wait_type: SemWaitType::Increase,
        nowait: false,
    };
    let zero = SemBlockedOp {
        semnum: 1,
        wait_type: SemWaitType::Zero,
        nowait: false,
    };
    let entry = Arc::new(SemQueueEntry::new(
        &[plain_sop(0, -1), plain_sop(1, 0)],
        None,
        waker,
        increase,
    ));
    set.update_blocker(&entry, zero);
    assert_eq!((set.ncnt(0), set.zcnt(1)), (0, 0));
    set.enqueue_waiter(entry.clone());
    assert_eq!((set.ncnt(0), set.zcnt(1)), (0, 1));
    set.update_blocker(&entry, increase);
    assert_eq!((set.ncnt(0), set.zcnt(1)), (1, 0));
    set.update_blocker(&entry, increase); // No double-accounting.
    let same_slot_zero = SemBlockedOp { semnum: 0, ..zero };
    set.update_blocker(&entry, same_slot_zero);
    assert_eq!((set.ncnt(0), set.zcnt(0)), (0, 1));
    for semnum in 0..2 {
        for (kind, cached) in [
            (SemWaitType::Increase, set.ncnt(semnum)),
            (SemWaitType::Zero, set.zcnt(semnum)),
        ] {
            let scanned = set
                .pending_const
                .iter()
                .chain(set.pending_alter.iter())
                .filter(|entry| entry.is_waiting_on(semnum, kind))
                .count();
            assert_eq!(cached, scanned);
        }
    }
    set.remove_waiter(&entry);
    set.remove_waiter(&entry);
    assert_eq!((set.ncnt(0), set.zcnt(0)), (0, 0));
    set.enqueue_waiter(entry.clone());
    let mut wakes = SemWakeBatch::default();
    set.complete_all_removed(&mut wakes);
    assert_eq!(
        (set.ncnt(0), set.zcnt(0), set.ncnt(1), set.zcnt(1)),
        (0, 0, 0, 0)
    );
    assert_eq!(entry.completed_result(), Some(Err(SystemError::EIDRM)));
}
