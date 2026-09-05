use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    ipc::sem::{SemId, SemManager},
    libs::spinlock::SpinLock,
    process::{namespace::ipc_namespace::IpcNamespace, pid::PidType, ProcessControlBlock},
};

#[derive(Debug)]
pub struct SemUndoAttachment {
    group: Arc<SemUndoGroup>,
}

#[derive(Debug)]
pub struct SemUndoGroup {
    ipc_ns: Weak<IpcNamespace>,
    #[allow(dead_code)]
    inner: SpinLock<SemUndoGroupState>,
}

#[allow(dead_code)]
#[derive(Debug)]
struct SemUndoGroupState {
    task_owners: usize,
    /// Published once in the bound IPC namespace's manager registry. Clearing
    /// records does not unregister a live group; only dead Weak entries expire.
    registry_registered: bool,
    records: Vec<SemUndoRecord>,
    reserved_records: usize,
    absence_generation: u64,
    retired: bool,
    records_taken: bool,
    #[cfg(test)]
    replay_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreparedSemUndoRecordState {
    Existing { expected_revision: u64 },
    Missing { expected_absence_generation: u64 },
    Live,
}

#[derive(Debug)]
struct PendingSemUndoRecordReservation {
    group: Weak<SemUndoGroup>,
    active: bool,
}

#[derive(Debug)]
pub(crate) struct SemUndoRecord {
    semid: SemId,
    adjustments: Box<[i16]>,
    revision: u64,
    prepared_state: PreparedSemUndoRecordState,
    reservation: Option<PendingSemUndoRecordReservation>,
}

pub(crate) enum PreparedSemUndoRecordAction<R> {
    Publish(R),
    Keep(R),
}

impl PendingSemUndoRecordReservation {
    fn new(group: &Arc<SemUndoGroup>) -> Self {
        Self {
            group: Arc::downgrade(group),
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for PendingSemUndoRecordReservation {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let Some(group) = self.group.upgrade() else {
            return;
        };
        let mut state = group.inner.lock_irqsave();
        state.reserved_records = state
            .reserved_records
            .checked_sub(1)
            .expect("SEM_UNDO record reservation count underflow");
        self.active = false;
    }
}

fn prepare_records_storage_capacity(
    records: &mut Vec<SemUndoRecord>,
    required_capacity: usize,
) -> Result<(), SystemError> {
    if records.capacity() >= required_capacity {
        return Ok(());
    }

    let additional = required_capacity
        .checked_sub(records.len())
        .ok_or(SystemError::ENOMEM)?;
    records
        .try_reserve_exact(additional)
        .map_err(|_| SystemError::ENOMEM)?;
    if records.capacity() < required_capacity {
        return Err(SystemError::ENOMEM);
    }
    Ok(())
}

impl SemUndoGroupState {
    fn bump_absence_generation(&mut self) {
        self.absence_generation = self.absence_generation.wrapping_add(1);
    }
}

impl SemUndoRecord {
    #[cfg(test)]
    fn new_live(semid: SemId, adjustments: Box<[i16]>) -> Self {
        Self {
            semid,
            adjustments,
            revision: 0,
            prepared_state: PreparedSemUndoRecordState::Live,
            reservation: None,
        }
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    fn publish_from(&mut self, record: &SemUndoRecord) {
        self.adjustments.copy_from_slice(&record.adjustments);
        self.bump_revision();
    }

    fn refresh_existing(&mut self, existing: &SemUndoRecord) {
        self.adjustments.copy_from_slice(&existing.adjustments);
        self.revision = existing.revision;
        self.prepared_state = PreparedSemUndoRecordState::Existing {
            expected_revision: existing.revision,
        };
    }

    fn refresh_missing(&mut self, absence_generation: u64) {
        self.adjustments.fill(0);
        self.revision = 0;
        self.prepared_state = PreparedSemUndoRecordState::Missing {
            expected_absence_generation: absence_generation,
        };
    }

    fn mark_live(&mut self) {
        self.prepared_state = PreparedSemUndoRecordState::Live;
    }

    pub(crate) fn adjustment(&self, semnum: usize) -> i16 {
        self.adjustments[semnum]
    }

    pub(crate) fn set_adjustment(&mut self, semnum: usize, adjustment: i16) {
        if self.adjustments[semnum] != adjustment {
            self.adjustments[semnum] = adjustment;
            self.bump_revision();
        }
    }

    pub(crate) fn clear_adjustment(&mut self, semnum: usize) {
        self.set_adjustment(semnum, 0);
    }

    pub(crate) fn clear_all_adjustments(&mut self) {
        if self.adjustments.iter().any(|&adjustment| adjustment != 0) {
            self.adjustments.fill(0);
            self.bump_revision();
        }
    }

    pub(crate) fn adjustment_count(&self) -> usize {
        self.adjustments.len()
    }

    #[cfg(test)]
    pub(crate) fn adjustment_for_test(&self, semnum: usize) -> i16 {
        self.adjustment(semnum)
    }

    #[cfg(test)]
    pub(crate) fn set_adjustment_for_test(&mut self, semnum: usize, adjustment: i16) {
        self.set_adjustment(semnum, adjustment);
    }
}

pub(crate) struct UnpublishedSemUndoAttachmentGuard {
    group: Arc<SemUndoGroup>,
    attachment: Option<SemUndoAttachment>,
    installed_child: Option<Arc<ProcessControlBlock>>,
    armed: bool,
}

impl SemUndoAttachment {
    pub(crate) fn new(group: Arc<SemUndoGroup>) -> Self {
        Self { group }
    }

    pub(crate) fn group(&self) -> Arc<SemUndoGroup> {
        self.group.clone()
    }

    #[cfg(test)]
    fn new_for_test(group: Arc<SemUndoGroup>) -> Self {
        Self::new(group)
    }

    #[cfg(test)]
    fn group_for_test(&self) -> Arc<SemUndoGroup> {
        self.group()
    }
}

impl Drop for SemUndoAttachment {
    // Replay is an explicit lifecycle operation and must never run from Drop.
    fn drop(&mut self) {}
}

impl SemUndoGroup {
    pub(crate) fn new(ipc_ns: &Arc<IpcNamespace>) -> Result<Arc<Self>, SystemError> {
        Arc::try_new(Self {
            ipc_ns: Arc::downgrade(ipc_ns),
            inner: SpinLock::new(SemUndoGroupState {
                task_owners: 1,
                registry_registered: false,
                records: Vec::new(),
                reserved_records: 0,
                absence_generation: 0,
                retired: false,
                records_taken: false,
                #[cfg(test)]
                replay_count: 0,
            }),
        })
        .map_err(|_| SystemError::ENOMEM)
    }

    /// The bound namespace's manager lock serializes registration callers.
    pub(crate) fn registry_registered(&self) -> bool {
        self.inner.lock_irqsave().registry_registered
    }

    /// Called only after the manager has successfully inserted its Weak entry.
    pub(crate) fn mark_registry_registered(&self) {
        self.inner.lock_irqsave().registry_registered = true;
    }

    pub(crate) fn verify_ipc_ns(&self, ipc_ns: &Arc<IpcNamespace>) -> Result<(), SystemError> {
        let state = self.inner.lock_irqsave();
        if state.task_owners == 0 || state.retired {
            return Err(SystemError::EINVAL);
        }
        if self.ipc_ns.ptr_eq(&Arc::downgrade(ipc_ns)) {
            Ok(())
        } else {
            Err(SystemError::EINVAL)
        }
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

        state.retired = true;
        true
    }

    fn take_retired_records(&self) -> Vec<SemUndoRecord> {
        let mut state = self.inner.lock_irqsave();
        if !state.retired || state.records_taken {
            return Vec::new();
        }
        state.records_taken = true;
        #[cfg(test)]
        {
            state.replay_count += 1;
        }
        core::mem::take(&mut state.records)
    }

    #[cfg(test)]
    fn new_for_test() -> Arc<Self> {
        Self::new(&crate::process::namespace::ipc_namespace::INIT_IPC_NAMESPACE).unwrap()
    }

    #[cfg(test)]
    fn new_for_test_bound_to_first_namespace() -> Arc<Self> {
        Self::new_for_test()
    }

    #[cfg(test)]
    pub(crate) fn new_for_test_bound_to(
        ipc_ns: &Arc<IpcNamespace>,
    ) -> Result<Arc<Self>, SystemError> {
        Self::new(ipc_ns)
    }

    #[allow(dead_code)]
    pub(crate) fn prepare_record(
        self: &Arc<Self>,
        semid: SemId,
        nsems: usize,
    ) -> Result<SemUndoRecord, SystemError> {
        let mut adjustments = Vec::new();
        adjustments
            .try_reserve_exact(nsems)
            .map_err(|_| SystemError::ENOMEM)?;
        adjustments.resize(nsems, 0);

        let mut reserved_storage = Vec::new();

        loop {
            let mut state = self.inner.lock_irqsave();
            if state.task_owners == 0 || state.retired {
                return Err(SystemError::EINVAL);
            }

            if let Some(existing) = state.records.iter().find(|item| item.semid == semid) {
                if existing.adjustments.len() != nsems {
                    return Err(SystemError::EINVAL);
                }
                adjustments.copy_from_slice(&existing.adjustments);
                return Ok(SemUndoRecord {
                    semid,
                    adjustments: adjustments.into_boxed_slice(),
                    revision: existing.revision,
                    prepared_state: PreparedSemUndoRecordState::Existing {
                        expected_revision: existing.revision,
                    },
                    reservation: None,
                });
            }

            let required_capacity = state
                .records
                .len()
                .checked_add(state.reserved_records)
                .and_then(|capacity| capacity.checked_add(1))
                .ok_or(SystemError::ENOMEM)?;

            if state.records.capacity() < required_capacity {
                if reserved_storage.capacity() < required_capacity {
                    drop(state);
                    prepare_records_storage_capacity(&mut reserved_storage, required_capacity)?;
                    continue;
                }

                reserved_storage.append(&mut state.records);
                core::mem::swap(&mut state.records, &mut reserved_storage);
            }

            state.reserved_records = state
                .reserved_records
                .checked_add(1)
                .ok_or(SystemError::ENOMEM)?;
            return Ok(SemUndoRecord {
                semid,
                adjustments: adjustments.into_boxed_slice(),
                revision: 0,
                prepared_state: PreparedSemUndoRecordState::Missing {
                    expected_absence_generation: state.absence_generation,
                },
                reservation: Some(PendingSemUndoRecordReservation::new(self)),
            });
        }
    }

    #[allow(dead_code)]
    pub(crate) fn commit_record(&self, record: SemUndoRecord) -> Result<(), SystemError> {
        self.commit_prepared_record_noalloc(record)
    }

    pub(crate) fn commit_prepared_record_noalloc(
        &self,
        record: SemUndoRecord,
    ) -> Result<(), SystemError> {
        self.with_prepared_record_noalloc(
            record,
            |_record| PreparedSemUndoRecordAction::Publish(()),
        )
        .map(|((), _)| ())
    }

    pub(crate) fn with_prepared_record_noalloc<R>(
        &self,
        mut record: SemUndoRecord,
        f: impl FnOnce(&mut SemUndoRecord) -> PreparedSemUndoRecordAction<R>,
    ) -> Result<(R, Option<SemUndoRecord>), SystemError> {
        let mut state = self.inner.lock_irqsave();
        if state.task_owners == 0 || state.retired {
            return Err(SystemError::EINVAL);
        }

        let existing_index = state
            .records
            .iter()
            .position(|item| item.semid == record.semid);

        if let Some(index) = existing_index {
            if state.records[index].adjustments.len() != record.adjustments.len() {
                return Err(SystemError::EINVAL);
            }
            let current_revision = state.records[index].revision;
            let stale = !matches!(
                record.prepared_state,
                PreparedSemUndoRecordState::Existing { expected_revision }
                    if current_revision == expected_revision
            ) && !matches!(record.prepared_state, PreparedSemUndoRecordState::Live);
            if stale {
                record.refresh_existing(&state.records[index]);
            }
            if let Some(mut reservation) = record.reservation.take() {
                state.reserved_records = state
                    .reserved_records
                    .checked_sub(1)
                    .expect("SEM_UNDO record reservation count underflow");
                reservation.disarm();
            }

            return match f(&mut record) {
                PreparedSemUndoRecordAction::Publish(result) => {
                    state.records[index].publish_from(&record);
                    Ok((result, None))
                }
                PreparedSemUndoRecordAction::Keep(result) => Ok((result, Some(record))),
            };
        }

        match record.prepared_state {
            PreparedSemUndoRecordState::Missing {
                expected_absence_generation,
            } if expected_absence_generation == state.absence_generation => {}
            PreparedSemUndoRecordState::Live => {}
            _ => record.refresh_missing(state.absence_generation),
        }

        if state.records.len() >= state.records.capacity() {
            return Err(SystemError::ENOMEM);
        }

        match f(&mut record) {
            PreparedSemUndoRecordAction::Publish(result) => {
                if let Some(mut reservation) = record.reservation.take() {
                    state.reserved_records = state
                        .reserved_records
                        .checked_sub(1)
                        .expect("SEM_UNDO record reservation count underflow");
                    reservation.disarm();
                }
                record.mark_live();
                state.records.push(record);
                state.bump_absence_generation();
                Ok((result, None))
            }
            PreparedSemUndoRecordAction::Keep(result) => Ok((result, Some(record))),
        }
    }

    pub(crate) fn with_record_mut<R>(
        &self,
        semid: SemId,
        f: impl FnOnce(&mut SemUndoRecord) -> R,
    ) -> Option<R> {
        let mut state = self.inner.lock_irqsave();
        state
            .records
            .iter_mut()
            .find(|record| record.semid == semid)
            .map(f)
    }

    pub(crate) fn remove_record(&self, semid: SemId) {
        let mut state = self.inner.lock_irqsave();
        state.records.retain(|record| record.semid != semid);
        state.bump_absence_generation();
    }

    #[cfg(test)]
    pub(crate) fn adjustment_for_test(&self, semid: SemId, semnum: usize) -> i16 {
        self.inner
            .lock_irqsave()
            .records
            .iter()
            .find(|record| record.semid == semid)
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
    ) -> Result<SemUndoRecord, SystemError> {
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
    fn verify_ipc_ns_for_test(&self, ipc_ns: Arc<IpcNamespace>) -> Result<(), SystemError> {
        self.verify_ipc_ns(&ipc_ns)
    }

    #[cfg(test)]
    pub(crate) fn insert_test_record(&self, semid: SemId, adjustments: &[i16]) {
        let mut state = self.inner.lock_irqsave();
        state.records.push(SemUndoRecord::new_live(
            semid,
            adjustments.to_vec().into_boxed_slice(),
        ));
        state.bump_absence_generation();
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
        state.records = Vec::with_capacity(capacity);
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

pub(crate) fn detach_sem_undo(pcb: &Arc<ProcessControlBlock>) {
    let Some(attachment) = pcb.take_sem_undo_attachment() else {
        return;
    };
    let group = attachment.group();
    drop(attachment);

    if !group.detach_owner_and_mark_last() {
        return;
    }

    replay_marked_records(pcb, &group);
}

fn replay_marked_records(pcb: &Arc<ProcessControlBlock>, group: &Arc<SemUndoGroup>) {
    let exiting_tgid = pcb.try_active_pid_ns().and_then(|pid_ns| {
        pcb.task_pid_nr_ns(PidType::TGID, Some(pid_ns))
            .filter(|tgid| tgid.data() != 0)?;
        pcb.task_pid_ptr(PidType::TGID)
    });

    let Some(ipc_ns) = group.ipc_ns.upgrade() else {
        for record in group.take_retired_records() {
            log::debug!(
                "dropping SEM_UNDO record for semid {} after IPC namespace teardown",
                record.semid.data()
            );
        }
        return;
    };

    let mut manager = ipc_ns.sem.lock();
    let records = group.take_retired_records();
    for record in records {
        SemManager::replay_sem_undo_adjustments(
            &mut manager,
            record.semid,
            &record.adjustments,
            exiting_tgid.clone(),
        );
    }
}

impl UnpublishedSemUndoAttachmentGuard {
    pub(crate) fn new(group: Arc<SemUndoGroup>) -> Self {
        {
            let mut state = group.inner.lock_irqsave();
            debug_assert!(
                state.task_owners > 0 && !state.retired,
                "SEM_UNDO shared owner must be acquired before final retirement"
            );
            state.task_owners = state
                .task_owners
                .checked_add(1)
                .expect("SEM_UNDO task owner count overflow");
        }

        Self {
            attachment: Some(SemUndoAttachment::new(group.clone())),
            group,
            installed_child: None,
            armed: true,
        }
    }

    pub(crate) fn install_into(&mut self, child: &ProcessControlBlock) {
        assert!(self.armed, "cannot install a disarmed SEM_UNDO guard");
        assert!(
            self.installed_child.is_none(),
            "SEM_UNDO guard can only be installed once"
        );
        let attachment = self
            .attachment
            .take()
            .expect("SEM_UNDO guard attachment token is missing");
        self.installed_child = Some(child.install_unpublished_sem_undo_attachment(attachment));
    }

    pub(crate) fn disarm(mut self) {
        debug_assert!(
            self.attachment.is_none() && self.installed_child.is_some(),
            "only an installed SEM_UNDO guard can be disarmed"
        );
        self.armed = false;
    }
}

impl Drop for UnpublishedSemUndoAttachmentGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        if let Some(child) = self.installed_child.take() {
            let attachment = child.take_sem_undo_attachment();
            debug_assert!(attachment.is_some(), "installed SEM_UNDO slot is empty");
            if let Some(attachment) = attachment {
                debug_assert!(Arc::ptr_eq(&attachment.group, &self.group));
                drop(attachment);
            }
        } else {
            drop(self.attachment.take());
        }

        let mut state = self.group.inner.lock_irqsave();
        debug_assert!(
            state.task_owners > 1,
            "unpublished SEM_UNDO rollback requires the parent owner"
        );
        state.task_owners -= 1;
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use super::{
        detach_sem_undo, PreparedSemUndoRecordState, SemUndoAttachment, SemUndoGroup,
        SemUndoRecord, UnpublishedSemUndoAttachmentGuard,
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
        assert_eq!(
            group.prepare_record_for_test(semid, 1),
            Err(SystemError::EINVAL)
        );
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
        assert_eq!(
            group.prepare_record_for_test(semid, 1),
            Err(SystemError::EINVAL)
        );
    }

    #[test]
    fn namespace_lifecycle_invariant_has_no_live_record_at_final_drop() {
        let ipc_ns = second_test_ipc_ns();
        let group = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
        let mut manager = ipc_ns.sem.lock();
        let semid = manager
            .semget(
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
    fn prepare_existing_record_copies_current_adjustments() {
        let group = SemUndoGroup::new_for_test();
        let semid = SemId::new(101);
        group.insert_test_record(semid, &[7, -3]);

        let record = group.prepare_record_for_test(semid, 2).unwrap();

        assert_eq!(record.adjustment_for_test(0), 7);
        assert_eq!(record.adjustment_for_test(1), -3);
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
        let mut stale = group.prepare_record_for_test(semid, 1).unwrap();
        stale.set_adjustment_for_test(0, 4);
        group.insert_test_record(semid, &[9]);

        assert_eq!(group.commit_record(stale), Ok(()));
        assert_eq!(group.adjustment_for_test(semid, 0), 9);
        assert_eq!(group.pending_record_reservations_for_test(), 0);
    }

    #[test]
    fn missing_record_reservation_loses_to_rmid_generation_with_retry() {
        let group = SemUndoGroup::new_for_test();
        let semid = SemId::new(207);
        let mut stale = group.prepare_record_for_test(semid, 1).unwrap();
        stale.set_adjustment_for_test(0, 4);
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
        let record = SemUndoRecord {
            semid: SemId::new(203),
            adjustments: alloc::vec![0].into_boxed_slice(),
            revision: 0,
            prepared_state: PreparedSemUndoRecordState::Live,
            reservation: None,
        };

        assert_eq!(group.commit_record(record), Err(SystemError::ENOMEM));
        assert_eq!(group.record_count_for_test(), 0);
        assert_eq!(group.record_capacity_for_test(), before_capacity);
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
}
