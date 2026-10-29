use super::lifecycle::replay_marked_records;
use super::*;
use crate::process::ProcessControlBlock;

impl SemUndoGroup {
    pub(super) fn new_for_test() -> Arc<Self> {
        Self::new(&crate::process::namespace::ipc_namespace::INIT_IPC_NAMESPACE).unwrap()
    }

    #[cfg(test)]
    pub(super) fn new_for_test_bound_to_first_namespace() -> Arc<Self> {
        Self::new_for_test()
    }

    #[cfg(test)]
    pub(crate) fn new_for_test_bound_to(
        ipc_ns: &Arc<IpcNamespace>,
    ) -> Result<Arc<Self>, SystemError> {
        Self::new(ipc_ns)
    }

    pub(crate) fn remove_record(&self, semid: SemId) {
        drop(self.take_record(semid));
    }

    #[cfg(test)]
    pub(crate) fn adjustment_for_test(&self, semid: SemId, semnum: usize) -> i16 {
        self.inner
            .lock_irqsave()
            .records
            .get(semid)
            .map(|record| record.adjustment(semnum))
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn has_live_records_in_namespace_for_test(
        &self,
        ipc_ns: &Arc<IpcNamespace>,
    ) -> bool {
        self.verify_ipc_ns(ipc_ns).is_ok() && !self.inner.lock_irqsave().records.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn prepare_record_for_test(
        self: &Arc<Self>,
        semid: SemId,
        nsems: usize,
    ) -> Result<PreparedSemUndoRecord, SystemError> {
        self.prepare_record(semid, nsems)
    }

    #[cfg(test)]
    pub(crate) fn task_owners_for_test(&self) -> usize {
        self.inner.lock_irqsave().task_owners
    }

    #[cfg(test)]
    pub(crate) fn replay_count_for_test(&self) -> usize {
        self.inner.lock_irqsave().replay_count
    }

    #[cfg(test)]
    pub(super) fn verify_ipc_ns_for_test(
        &self,
        ipc_ns: Arc<IpcNamespace>,
    ) -> Result<(), SystemError> {
        self.verify_ipc_ns(&ipc_ns)
    }

    #[cfg(test)]
    pub(crate) fn insert_test_record(&self, semid: SemId, adjustments: &[i16]) {
        let mut state = self.inner.lock_irqsave();
        let required = state.records.len() + 1;
        let mut spare = UndoRecords::default();
        spare.prepare(required).unwrap();
        state.records.install_prepared(&mut spare, required);
        state.records.push_prepared(SemUndoRecord::new_live(
            semid,
            adjustments.to_vec().into_boxed_slice(),
        ));
    }

    #[cfg(test)]
    pub(crate) fn record_count_for_test(&self) -> usize {
        self.inner.lock_irqsave().records.len()
    }

    #[cfg(test)]
    pub(crate) fn record_capacity_for_test(&self) -> usize {
        self.inner.lock_irqsave().records.capacity()
    }

    #[cfg(test)]
    pub(crate) fn set_record_capacity_for_test(&self, capacity: usize) {
        let mut state = self.inner.lock_irqsave();
        assert!(state.records.is_empty());
        state.records = UndoRecords::default();
        state.records.prepare(capacity).unwrap();
        assert_eq!(state.records.capacity(), capacity);
    }

    #[cfg(test)]
    pub(crate) fn pending_record_reservations_for_test(&self) -> usize {
        self.inner.lock_irqsave().reserved_records
    }

    #[cfg(test)]
    pub(crate) fn detach_last_owner_for_test(&self) -> bool {
        self.detach_owner_and_mark_last()
    }

    #[cfg(test)]
    pub(crate) fn replay_marked_records_for_test(self: &Arc<Self>, pcb: &Arc<ProcessControlBlock>) {
        replay_marked_records(pcb, self);
    }
}
