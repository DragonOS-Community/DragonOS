use super::*;
use crate::ipc::sem::manager::SemManager;
use crate::ipc::sem::set::test_support::*;

#[test]
fn new_manager_starts_with_empty_undo_registry() {
    assert!(SemManager::new().id2sem.is_empty());
}

#[test]
fn create_tables_require_both_spares_and_recheck_growth() {
    let mut manager = SemManager::new();
    let key = SemKey::new(153);
    let mut ids = HashMap::new();
    let mut keys = HashMap::new();
    ids.try_reserve(4).unwrap();
    assert_eq!(
        manager.install_create_tables(key, &mut ids, &mut keys),
        Err((0, 4))
    );
    assert_eq!(manager.id2sem.capacity(), 0);
    assert_eq!(manager.key2id.capacity(), 0);
    keys.try_reserve(4).unwrap();
    manager
        .install_create_tables(key, &mut ids, &mut keys)
        .unwrap();
    while manager.id2sem.len() < manager.id2sem.capacity() {
        let next_key = SemKey::new(200 + manager.id2sem.len());
        insert_test_set(&mut manager, next_key, &[3]);
    }
    let count = manager.id2sem.len();
    assert_eq!(manager.key2id.len(), count);
    ids.try_reserve(count + 1).unwrap();
    keys.try_reserve(count + 1).unwrap();
    // Competing creations can consume the prepared headroom before the
    // caller reacquires the lock. Neither live table may be moved yet.
    while manager.id2sem.len() < ids.capacity() {
        let next_key = SemKey::new(200 + manager.id2sem.len());
        insert_test_set(&mut manager, next_key, &[3]);
    }
    let live_count = manager.id2sem.len();
    let capacities = (manager.id2sem.capacity(), manager.key2id.capacity());
    let (need_ids, need_keys) = manager
        .install_create_tables(key, &mut ids, &mut keys)
        .unwrap_err();
    assert_eq!(
        capacities,
        (manager.id2sem.capacity(), manager.key2id.capacity())
    );
    assert_eq!(manager.id2sem.len(), live_count);
    ids.try_reserve(need_ids).unwrap();
    keys.try_reserve(need_keys).unwrap();
    manager
        .install_create_tables(key, &mut ids, &mut keys)
        .unwrap();
    assert_eq!(manager.id2sem.len(), live_count);
    assert_eq!(manager.key2id.len(), live_count);
    assert!(manager.id2sem.capacity() > live_count);
    assert!(manager.key2id.capacity() > live_count);
    assert!(ids.is_empty() && keys.is_empty());
    assert_eq!((ids.capacity(), keys.capacity()), capacities);
    for id in manager.key2id.values() {
        assert_eq!(
            manager
                .get_by_semid_checked(*id)
                .unwrap()
                .get_value(0, SemCtlCmd::GetVal)
                .unwrap(),
            3
        );
    }
}

#[test]
fn semget_lookup_validates_before_preparing_storage() {
    let mut manager = SemManager::new();
    let key = SemKey::new(154);
    insert_test_set(&mut manager, key, &[0]);
    assert_eq!(
        manager.lookup_semget(key, SEMMSL + 1, SemFlags::IPC_CREAT | SemFlags::IPC_EXCL),
        Err(SystemError::EINVAL)
    );
    assert_eq!(
        manager.lookup_semget(key, 2, SemFlags::IPC_CREAT | SemFlags::IPC_EXCL),
        Err(SystemError::EEXIST)
    );
    assert_eq!(
        manager.lookup_semget(key, 2, SemFlags::empty()),
        Err(SystemError::EINVAL)
    );
    assert_eq!(
        manager.lookup_semget(SemKey::new(155), 0, SemFlags::empty()),
        Err(SystemError::ENOENT)
    );
    assert_eq!(
        manager.lookup_semget(SemKey::new(155), 0, SemFlags::IPC_CREAT),
        Err(SystemError::EINVAL)
    );
    assert_eq!(
        manager.lookup_semget(IPC_PRIVATE, 1, SemFlags::IPC_EXCL),
        Ok(None)
    );
    manager.total_sems = SEMMNS;
    assert_eq!(
        manager.lookup_semget(IPC_PRIVATE, 1, SemFlags::empty()),
        Err(SystemError::ENOSPC)
    );
    assert_eq!(
        manager.lookup_semget(IPC_PRIVATE, 0, SemFlags::empty()),
        Err(SystemError::EINVAL)
    );
}

#[test]
fn private_create_never_prepares_key_table() {
    let mut manager = SemManager::new();
    let mut ids = HashMap::new();
    let mut keys = HashMap::new();
    assert_eq!(
        manager.install_create_tables(IPC_PRIVATE, &mut ids, &mut keys),
        Err((4, 0))
    );
    ids.try_reserve(4).unwrap();
    manager
        .install_create_tables(IPC_PRIVATE, &mut ids, &mut keys)
        .unwrap();
    assert!(manager.id2sem.capacity() > 0);
    assert_eq!(manager.key2id.capacity(), 0);
    assert_eq!(keys.capacity(), 0);
}

#[test]
fn prepared_setall_token_returns_eidrm_after_rmid() {
    let mut manager = SemManager::new();
    let id = insert_test_set(&mut manager, SemKey::new(11), &[1, 2]);
    let token = SemSetAllToken::new(id, 2);

    remove_test_set(&mut manager, id);

    assert_eq!(
        manager.setall(token, &[7, 8], &mut SemWakeBatch::default()),
        Err(SystemError::EIDRM)
    );
}

#[test]
fn prepared_setall_range_error_precedes_removal() {
    let mut manager = SemManager::new();
    let id = insert_test_set(&mut manager, SemKey::new(12), &[1, 2]);
    let token = SemSetAllToken::new(id, 2);
    remove_test_set(&mut manager, id);
    assert_eq!(
        manager.setall(token, &[7, 32768], &mut SemWakeBatch::default()),
        Err(SystemError::ERANGE)
    );
}

#[test]
fn stale_prepared_setall_token_does_not_modify_reused_index() {
    let mut manager = SemManager::new();
    let old_id = insert_test_set(&mut manager, SemKey::new(21), &[1, 2]);
    let token = SemSetAllToken::new(old_id, 2);

    remove_test_set(&mut manager, old_id);
    let new_id = insert_test_set(&mut manager, SemKey::new(22), &[3, 4]);
    assert_ne!(old_id, new_id);
    assert_eq!(
        old_id.data() & IpcIdAllocator::IPC_ID_IDX_MASK,
        new_id.data() & IpcIdAllocator::IPC_ID_IDX_MASK
    );

    assert_eq!(
        manager.setall(token, &[7, 8], &mut SemWakeBatch::default()),
        Err(SystemError::EIDRM)
    );
    assert_eq!(sem_values(&manager, new_id), vec![3, 4]);
}
