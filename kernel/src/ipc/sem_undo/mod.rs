//! SEM_UNDO group state. The manager lock precedes the group lock whenever both are needed.
use crate::{
    ipc::sem::SemId, libs::spinlock::SpinLock, process::namespace::ipc_namespace::IpcNamespace,
};
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

mod lifecycle;
mod record;
mod storage;
pub use lifecycle::SemUndoAttachment;
pub(crate) use lifecycle::{
    detach_sem_undo, PendingSemUndoReplay, UnpublishedSemUndoAttachmentGuard,
};
use record::PendingSemUndoRecordReservation;
pub(crate) use record::{PreparedSemUndoRecord, PreparedSemUndoRecordAction, SemUndoRecord};
use storage::UndoRecords;

#[derive(Debug)]
pub struct SemUndoGroup {
    ipc_ns: Weak<IpcNamespace>,
    inner: SpinLock<SemUndoGroupState>,
}

#[derive(Debug)]
struct SemUndoGroupState {
    task_owners: usize,
    records: UndoRecords,
    reserved_records: usize,
    phase: UndoPhase,
    #[cfg(test)]
    replay_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UndoPhase {
    Active,
    Retired,
    Replaying,
}

impl SemUndoGroup {
    pub(crate) fn new(ipc_ns: &Arc<IpcNamespace>) -> Result<Arc<Self>, SystemError> {
        Arc::try_new(Self {
            ipc_ns: Arc::downgrade(ipc_ns),
            inner: SpinLock::new(SemUndoGroupState {
                task_owners: 1,
                records: UndoRecords::default(),
                reserved_records: 0,
                phase: UndoPhase::Active,
                #[cfg(test)]
                replay_count: 0,
            }),
        })
        .map_err(|_| SystemError::ENOMEM)
    }

    pub(crate) fn verify_ipc_ns(&self, ipc_ns: &Arc<IpcNamespace>) -> Result<(), SystemError> {
        let state = self.inner.lock_irqsave();
        if state.task_owners == 0 || state.phase != UndoPhase::Active {
            return Err(SystemError::EINVAL);
        }
        if self.ipc_ns.ptr_eq(&Arc::downgrade(ipc_ns)) {
            Ok(())
        } else {
            Err(SystemError::EINVAL)
        }
    }

    fn acquire_shared_owner(&self) {
        let mut state = self.inner.lock_irqsave();
        debug_assert!(
            state.task_owners > 0 && state.phase == UndoPhase::Active,
            "SEM_UNDO shared owner must be acquired before final retirement"
        );
        state.task_owners = state
            .task_owners
            .checked_add(1)
            .expect("SEM_UNDO task owner count overflow");
    }

    fn rollback_unpublished_owner(&self) {
        let mut state = self.inner.lock_irqsave();
        debug_assert!(
            state.task_owners > 1,
            "unpublished SEM_UNDO rollback requires the parent owner"
        );
        state.task_owners -= 1;
    }

    fn detach_owner_and_mark_last(&self) -> bool {
        let mut state = self.inner.lock_irqsave();
        debug_assert!(
            state.task_owners > 0,
            "SEM_UNDO detach requires an attached task owner"
        );
        if state.task_owners == 0 {
            return false;
        }

        state.task_owners -= 1;
        if state.task_owners != 0 {
            return false;
        }

        state.phase = UndoPhase::Retired;
        true
    }

    /// Claim retirement exactly once without hiding pending debt from semctl.
    fn begin_replay(&self) -> bool {
        let mut state = self.inner.lock_irqsave();
        if state.phase != UndoPhase::Retired {
            return false;
        }
        state.phase = UndoPhase::Replaying;
        #[cfg(test)]
        {
            state.replay_count += 1;
        }
        true
    }

    /// The caller holds the namespace manager lock until this debt is applied.
    /// Other records remain visible to SETVAL/SETALL and IPC_RMID between steps.
    fn pop_retired_record(&self) -> Option<SemUndoRecord> {
        let mut state = self.inner.lock_irqsave();
        debug_assert!(state.phase == UndoPhase::Replaying);
        state.records.pop()
    }

    /// Only valid after the bound namespace can no longer be upgraded.
    fn discard_retired_records(&self) -> UndoRecords {
        let mut state = self.inner.lock_irqsave();
        debug_assert!(state.phase == UndoPhase::Replaying);
        core::mem::take(&mut state.records)
    }

    pub(crate) fn prepare_record(
        self: &Arc<Self>,
        semid: SemId,
        nsems: usize,
    ) -> Result<PreparedSemUndoRecord, SystemError> {
        let mut adjustments = Vec::new();
        let mut reserved_storage = UndoRecords::default();

        loop {
            let mut state = self.inner.lock_irqsave();
            if state.task_owners == 0 || state.phase != UndoPhase::Active {
                return Err(SystemError::EINVAL);
            }

            if let Some(existing) = state.records.get(semid) {
                if existing.adjustments.len() != nsems {
                    return Err(SystemError::EINVAL);
                }
                return Ok(PreparedSemUndoRecord {
                    semid,
                    nsems,
                    candidate: None,
                    reservation: None,
                });
            }

            if adjustments.len() != nsems {
                drop(state);
                adjustments
                    .try_reserve_exact(nsems)
                    .map_err(|_| SystemError::ENOMEM)?;
                adjustments.resize(nsems, 0);
                continue;
            }

            let required_capacity = state
                .records
                .len()
                .checked_add(state.reserved_records)
                .and_then(|capacity| capacity.checked_add(1))
                .ok_or(SystemError::ENOMEM)?;

            if !state.records.can_hold(required_capacity) {
                // Rebuild both together even when only hash tombstones exhaust
                // insertion capacity. Dense capacity also bounds index residency.
                let capacity = state.records.capacity();
                let target = if capacity < required_capacity {
                    required_capacity.max(capacity.saturating_mul(2)).max(4)
                } else {
                    capacity
                };
                if !reserved_storage.can_hold(target) {
                    drop(state);
                    reserved_storage.prepare(target)?;
                    continue;
                }
                state
                    .records
                    .install_prepared(&mut reserved_storage, required_capacity);
            }

            state.reserved_records = state
                .reserved_records
                .checked_add(1)
                .ok_or(SystemError::ENOMEM)?;
            // into_boxed_slice may shrink an allocation. Do it after releasing
            // the group lock, with an armed reservation already owning the slot.
            let reservation = PendingSemUndoRecordReservation::new(self);
            drop(state);
            return Ok(PreparedSemUndoRecord {
                semid,
                nsems,
                candidate: Some(SemUndoRecord {
                    semid,
                    adjustments: adjustments.into_boxed_slice(),
                }),
                reservation: Some(reservation),
            });
        }
    }

    #[cfg(test)]
    pub(crate) fn commit_record(&self, record: PreparedSemUndoRecord) -> Result<(), SystemError> {
        self.commit_prepared_record_noalloc(record)
    }

    #[cfg(test)]
    pub(crate) fn commit_prepared_record_noalloc(
        &self,
        record: PreparedSemUndoRecord,
    ) -> Result<(), SystemError> {
        self.with_prepared_record_noalloc(record, |_record| {
            PreparedSemUndoRecordAction::Complete(())
        })
        .map(|((), _)| ())
    }

    /// Borrow the current record under the group lock, never a stale snapshot.
    /// The callback must simulate without writes and mutate only on Complete;
    /// Keep (including errors/blocked operations) must leave the debt unchanged.
    /// First use publishes a zero record before calling the simulation, as in
    /// Linux find_alloc_undo; Keep retains only a lightweight existing token.
    /// SemopScratch provides that separation without an additional transaction.
    pub(crate) fn with_prepared_record_noalloc<R>(
        &self,
        mut record: PreparedSemUndoRecord,
        f: impl FnOnce(&mut SemUndoRecord) -> PreparedSemUndoRecordAction<R>,
    ) -> Result<(R, Option<PreparedSemUndoRecord>), SystemError> {
        // A competing first publisher makes this candidate redundant. Keep
        // its disposal outside the group lock and return a lightweight token.
        let mut _retired_candidate = None;
        let mut state = self.inner.lock_irqsave();
        if state.task_owners == 0 || state.phase != UndoPhase::Active {
            return Err(SystemError::EINVAL);
        }

        if let Some(existing) = state.records.get(record.semid) {
            if existing.adjustments.len() != record.nsems {
                return Err(SystemError::EINVAL);
            }
            _retired_candidate = record.candidate.take();
            if let Some(mut reservation) = record.reservation.take() {
                state.reserved_records = state
                    .reserved_records
                    .checked_sub(1)
                    .expect("SEM_UNDO record reservation count underflow");
                reservation.disarm();
            }

            return match f(state.records.get_mut(record.semid).unwrap()) {
                PreparedSemUndoRecordAction::Complete(result) => Ok((result, None)),
                PreparedSemUndoRecordAction::Keep(result) => Ok((result, Some(record))),
            };
        }

        // Like Linux find_alloc_undo, publish a zero record before simulating:
        // even a blocked/failed operation retains this group/set association.
        // Existing tokens cannot recreate a record removed by RMID.
        if record.candidate.is_none() {
            return Err(SystemError::EINVAL);
        }
        if !state.records.can_hold(state.records.len() + 1) {
            return Err(SystemError::ENOMEM);
        }
        if let Some(mut reservation) = record.reservation.take() {
            state.reserved_records = state
                .reserved_records
                .checked_sub(1)
                .expect("SEM_UNDO record reservation count underflow");
            reservation.disarm();
        }
        state
            .records
            .push_prepared(record.candidate.take().unwrap());
        match f(state.records.get_mut(record.semid).unwrap()) {
            PreparedSemUndoRecordAction::Complete(result) => Ok((result, None)),
            PreparedSemUndoRecordAction::Keep(result) => Ok((result, Some(record))),
        }
    }

    pub(crate) fn with_record_mut<R>(
        &self,
        semid: SemId,
        f: impl FnOnce(&mut SemUndoRecord) -> R,
    ) -> Option<R> {
        let mut state = self.inner.lock_irqsave();
        state.records.get_mut(semid).map(f)
    }

    pub(crate) fn take_record(&self, semid: SemId) -> Option<SemUndoRecord> {
        let mut state = self.inner.lock_irqsave();
        state.records.remove(semid)
    }

    /// Best-effort reclamation. Caller must hold neither manager nor group lock.
    /// Preparation and old-buffer disposal stay outside both critical sections.
    pub(crate) fn shrink_records(&self) {
        let mut spare = UndoRecords::default();
        let mut retired = UndoRecords::default();
        let needed = {
            let mut state = self.inner.lock_irqsave();
            state.shrink_records_prepared(&mut spare, &mut retired)
        };
        if needed == 0 || spare.prepare(needed).is_err() {
            return;
        }
        let mut state = self.inner.lock_irqsave();
        state.shrink_records_prepared(&mut spare, &mut retired);
    }
}

impl SemUndoGroupState {
    /// Pending Missing tokens own capacity even when no live records remain.
    /// Recheck after unlocked allocation: another prepare may need more slots.
    fn shrink_records_prepared(
        &mut self,
        spare: &mut UndoRecords,
        retired: &mut UndoRecords,
    ) -> usize {
        debug_assert!(spare.is_empty());
        debug_assert!(retired.is_empty() && retired.capacity() == 0);
        let Some(required) = self.records.len().checked_add(self.reserved_records) else {
            return 0;
        };
        if required == 0 {
            core::mem::swap(&mut self.records, retired);
            return 0;
        }
        if self.records.capacity() <= 4 || required > self.records.capacity() / 4 {
            return 0;
        }
        if !spare.can_hold(required) {
            return required.saturating_mul(2).max(4);
        }
        if spare.capacity() < self.records.capacity() {
            self.records.install_prepared(spare, required);
        }
        0
    }
}

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
