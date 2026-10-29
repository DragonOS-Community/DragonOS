use super::*;
use crate::ipc::sem::set::test_support::test_perm;
use crate::ipc::sem::set::SemUndoRegistry;
use alloc::sync::Weak;

impl SemManager {
    /// Test-only setup; production reserves registry storage before locking.
    #[cfg(test)]
    pub(in crate::ipc::sem) fn ensure_undo_group_registered(
        &mut self,
        group: &Arc<SemUndoGroup>,
        semid: SemId,
    ) -> Result<(), SystemError> {
        let set = self.get_by_semid_checked_mut(semid)?;
        let mut spare = SemUndoRegistry::default();
        if let Err(capacity) = set.ensure_undo_group_registered_prepared(group, &mut spare) {
            spare.prepare(capacity)?;
            set.ensure_undo_group_registered_prepared(group, &mut spare)
                .unwrap();
        }
        Ok(())
    }

    pub(in crate::ipc::sem) fn update_queue_for_test(&mut self, semid: SemId) {
        let Ok(set) = self.get_by_semid_checked_mut(semid) else {
            return;
        };
        set.update_queue(&mut SemWakeBatch::default());
    }

    #[cfg(test)]
    pub(in crate::ipc::sem) fn live_undo_group_count_for_test(&self) -> usize {
        let mut groups: Vec<Weak<SemUndoGroup>> = Vec::new();
        for weak in self
            .id2sem
            .values()
            .flat_map(|set| set.undo_groups_for_test())
        {
            if weak.strong_count() != 0 && !groups.iter().any(|old| old.ptr_eq(weak)) {
                groups.push(weak.clone());
            }
        }
        groups.len()
    }

    #[cfg(test)]
    pub(in crate::ipc::sem) fn undo_registry_contains_for_test(
        &self,
        group: &Arc<SemUndoGroup>,
    ) -> bool {
        self.id2sem
            .values()
            .flat_map(|set| set.undo_groups_for_test())
            .any(|weak| weak.ptr_eq(&Arc::downgrade(group)))
    }

    #[cfg(test)]
    pub(crate) fn namespace_lifecycle_invariant_for_test(&self) -> bool {
        self.id2sem
            .values()
            .flat_map(|set| set.undo_groups_for_test())
            .all(|weak| {
                weak.upgrade()
                    .is_none_or(|group| group.record_count_for_test() == 0)
            })
    }

    #[cfg(test)]
    pub(crate) fn prepare_undo_record_and_registry_for_test(
        &mut self,
        group: &Arc<SemUndoGroup>,
        semid: SemId,
    ) -> Result<(), SystemError> {
        let nsems = self.validate_semid_nsems(semid)?;
        let record = group.prepare_record(semid, nsems)?;
        self.ensure_undo_group_registered(group, semid)?;
        group.commit_prepared_record_noalloc(record)
    }

    pub(in crate::ipc::sem) fn insert_test_set(&mut self, key: SemKey, vals: &[i32]) -> SemId {
        let ipc_id = self.id_allocator.alloc().unwrap();
        let id = SemId::new(ipc_id.raw);
        let set = KernelSemSet::new_for_test(test_perm(id, key, ipc_id.seq), vals);
        self.key2id.insert(key, id);
        self.id2sem.insert(ipc_id.idx, set);
        self.total_sems += vals.len();
        id
    }
    pub(in crate::ipc::sem) fn remove_test_set(&mut self, id: SemId) {
        let decoded = IpcIdAllocator::decode(id.data()).unwrap();
        let set = self.id2sem.remove(&decoded.idx).unwrap();
        self.key2id.remove(&SemKey::new(set.permissions().key));
        self.id_allocator.free_idx(decoded.idx);
        self.total_sems = self.total_sems.saturating_sub(set.nsems());
    }
    pub(in crate::ipc::sem) fn reset_allocator_for_test(&mut self, capacity: usize) {
        self.id_allocator = IpcIdAllocator::new(capacity).unwrap();
    }
    pub(in crate::ipc::sem) fn clear_undo_for_setval(&mut self, id: SemId, semnum: usize) {
        if let Ok(set) = self.get_by_semid_checked_mut(id) {
            set.clear_undo_for_setval(id, semnum);
        }
    }
    pub(in crate::ipc::sem) fn clear_undo_for_setall(&mut self, id: SemId) {
        if let Ok(set) = self.get_by_semid_checked_mut(id) {
            set.clear_undo_for_setall(id);
        }
    }
}
