//! semtimedop preparation, identity rechecks, waiting and cancellation.
use crate::{
    ipc::{ipc_perm, sem_undo::PreparedSemUndoRecordAction},
    libs::wait_queue::{TimeoutWaker, Waiter},
    process::{namespace::ipc_namespace::IpcNamespace, pid::PidType, ProcessManager},
    time::{
        timer::{clock, next_n_us_timer_jiffies, Timer},
        Duration,
    },
};
use alloc::sync::Arc;
use system_error::SystemError;

use super::{
    abi::*,
    manager::SemManager,
    set::{
        SemAttempt, SemBlockedOp, SemQueueEntry, SemUndoRegistry, SemWaitType, SemWakeBatch,
        SemopScratch,
    },
};
impl SemManager {
    pub(super) fn cancel_queued_entry(
        &mut self,
        semid: SemId,
        entry: &Arc<SemQueueEntry>,
        error: SystemError,
    ) -> Result<usize, SystemError> {
        if let Some(result) = entry.completed_result() {
            return result;
        }

        if let Ok(set) = self.get_by_semid_checked_mut(semid) {
            if let Some(result) = entry.completed_result() {
                return result;
            }
            if set.finish_waiter(entry, Err(error.clone())) {
                return Err(error);
            }
            return entry
                .completed_result()
                .expect("completed semaphore queue entry lost its terminal result");
        }

        if let Some(result) = entry.completed_result() {
            return result;
        }
        if entry.complete(Err(SystemError::EIDRM)) {
            return Err(SystemError::EIDRM);
        }
        entry
            .completed_result()
            .expect("completed semaphore queue entry lost its terminal result")
    }

    /// # semtimedop: execute `sops` atomically, blocking if necessary
    ///
    /// This function manages the lock internally (it must release it while waiting);
    /// callers must not hold the `ipcns.sem` lock in advance.
    ///
    /// - `timeout == None`: wait indefinitely (equivalent to `semop`)
    /// - `timeout == Some(Duration::ZERO)`: do not block
    /// - Otherwise: block until timeout and return EAGAIN
    pub fn semtimedop(
        ipcns: &Arc<IpcNamespace>,
        semid: SemId,
        sops: &[PosixSemBuf],
        timeout: Option<Duration>,
    ) -> Result<usize, SystemError> {
        if sops.is_empty() {
            return Err(SystemError::EINVAL);
        }
        if sops.len() > SEMOPM {
            return Err(SystemError::E2BIG);
        }

        let non_blocking = timeout == Some(Duration::ZERO);
        let has_undo = sops
            .iter()
            .any(|op| (op.sem_flg as u32) & SemFlags::SEM_UNDO.bits() != 0);
        // Check read permission only for all-zero waits; otherwise check write permission
        // to match Linux semantics.
        let alter = sops.iter().any(|op| op.sem_op != 0);

        let target_user_ns = ipcns.user_ns.clone();
        {
            let guard = ipcns.sem.lock();
            let set = guard.get_by_semid_checked(semid)?;
            // Match Linux: check semnum bounds (EFBIG) before permissions (EACCES).
            if sops.iter().any(|op| op.sem_num as usize >= set.nsems()) {
                return Err(SystemError::EFBIG);
            }
            ipc_perm::ipc_permission(
                set.permissions(),
                if alter {
                    Self::IPC_WRITE
                } else {
                    Self::IPC_READ
                },
                &target_user_ns,
            )?;
        }

        let deadline_ticks = timeout.map(|d| next_n_us_timer_jiffies(d.total_micros()));
        let (waiter, waker) = Waiter::new_pair();
        let timer =
            deadline_ticks.map(|deadline| Timer::new(TimeoutWaker::new(waker.clone()), deadline));

        let current = ProcessManager::current_pcb();
        let pid = current.task_pid_ptr(PidType::TGID);
        let undo_group = if has_undo {
            Some(current.ensure_sem_undo_group(ipcns)?)
        } else {
            None
        };
        // A cancelled first-use reservation can outlive the last RMID shrink.
        // Declare cleanup before entries/guards so their reservations are gone
        // before any allocation or buffer disposal performed by reclamation.
        let prepared_missing = core::cell::Cell::new(false);
        defer::defer!({
            if prepared_missing.get() {
                if let Some(group) = undo_group.as_ref() {
                    group.shrink_records();
                }
            }
        });
        let mut immediate_scratch = SemopScratch::try_new(sops)?;
        let plain_prepared_entry = if has_undo {
            None
        } else {
            Some(
                Arc::try_new(SemQueueEntry::new_prepared(
                    SemQueueEntry::prepare_sops(sops)?,
                    pid.clone(),
                    None,
                    None,
                    waker.clone(),
                    SemopScratch::try_new(sops)?,
                    SemBlockedOp {
                        semnum: 0,
                        wait_type: SemWaitType::Zero,
                        nowait: false,
                    },
                ))
                .map_err(|_| SystemError::ENOMEM)?,
            )
        };

        let mut wakes = SemWakeBatch::default();
        let mut registry_spare = SemUndoRegistry::default();
        let mut registry_capacity_needed = 0;
        let entry = loop {
            // Revalidate after unlocked undo-registry preparation.
            if registry_capacity_needed != 0 {
                registry_spare.prepare(registry_capacity_needed)?;
                registry_capacity_needed = 0;
            }
            let nsems = {
                let guard = ipcns.sem.lock();
                let set = guard.get_by_semid_checked(semid)?;
                // Match Linux: check semnum bounds (EFBIG) before permissions (EACCES).
                if sops.iter().any(|op| op.sem_num as usize >= set.nsems()) {
                    return Err(SystemError::EFBIG);
                }
                ipc_perm::ipc_permission(
                    set.permissions(),
                    if alter {
                        Self::IPC_WRITE
                    } else {
                        Self::IPC_READ
                    },
                    &target_user_ns,
                )?;
                set.nsems()
            };

            let prepared_undo = if let Some(group) = undo_group.as_ref() {
                let record = group.prepare_record(semid, nsems)?;
                prepared_missing.set(prepared_missing.get() || !record.was_existing());
                let entry = Arc::try_new(SemQueueEntry::new_prepared(
                    SemQueueEntry::prepare_sops(sops)?,
                    pid.clone(),
                    Some(group.clone()),
                    Some(record),
                    waker.clone(),
                    SemopScratch::try_new(sops)?,
                    SemBlockedOp {
                        semnum: 0,
                        wait_type: SemWaitType::Zero,
                        nowait: false,
                    },
                ))
                .map_err(|_| SystemError::ENOMEM)?;
                Some(entry)
            } else {
                None
            };

            let mut guard = ipcns.sem.lock();
            if guard.validate_semid_nsems(semid)? != nsems {
                continue;
            }
            if let Some(prepared_entry) = prepared_undo {
                let mut record_slot = prepared_entry.undo_record.lock_irqsave();
                let prepared_record = record_slot
                    .take()
                    .expect("prepared SEM_UNDO entry owns its record");
                if prepared_record.adjustment_count() != nsems {
                    return Err(SystemError::EINVAL);
                }
                // First-use candidates must be associated before zero-record
                // publication. Queued operations retain Existing tokens only.
                // Full semid was rechecked above, so Existing cannot refer to a
                // destroyed/reused set. SETVAL/SETALL retain live associations.
                let already_associated = prepared_record.was_existing();
                if !already_associated {
                    let set = guard.get_by_semid_checked_mut(semid)?;
                    if let Err(capacity) = set.ensure_undo_group_registered_prepared(
                        undo_group
                            .as_ref()
                            .expect("SEM_UNDO operation has a current group"),
                        &mut registry_spare,
                    ) {
                        registry_capacity_needed = capacity;
                        continue;
                    }
                }
                let set = guard.get_by_semid_checked_mut(semid)?;
                let (outcome, kept_record) = undo_group
                    .as_ref()
                    .expect("SEM_UNDO operation has a current group")
                    .with_prepared_record_noalloc(prepared_record, |record| {
                        let outcome =
                            set.try_apply(sops, pid.clone(), Some(record), &mut immediate_scratch);
                        match outcome {
                            Ok(completed @ SemAttempt::Completed { .. }) => {
                                PreparedSemUndoRecordAction::Complete(Ok(completed))
                            }
                            Ok(SemAttempt::Blocked(blocker)) => {
                                PreparedSemUndoRecordAction::Keep(Ok(SemAttempt::Blocked(blocker)))
                            }
                            Err(error) => PreparedSemUndoRecordAction::Keep(Err(error)),
                        }
                    })?;
                *record_slot = kept_record;
                match outcome? {
                    SemAttempt::Completed {
                        waiter_state_changed,
                    } => {
                        drop(record_slot);
                        if waiter_state_changed {
                            set.update_queue(&mut wakes);
                        }
                        return Ok(0);
                    }
                    SemAttempt::Blocked(blocker) => {
                        if blocker.nowait || non_blocking {
                            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                        }
                        if deadline_ticks.is_some_and(|deadline| clock() >= deadline) {
                            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                        }
                        drop(record_slot);
                        set.update_blocker(&prepared_entry, blocker);
                        set.enqueue_waiter(prepared_entry.clone());
                        break prepared_entry;
                    }
                }
            } else {
                let set = guard.get_by_semid_checked_mut(semid)?;
                match set.try_apply(sops, pid.clone(), None, &mut immediate_scratch)? {
                    SemAttempt::Completed {
                        waiter_state_changed,
                    } => {
                        if waiter_state_changed {
                            set.update_queue(&mut wakes);
                        }
                        return Ok(0);
                    }
                    SemAttempt::Blocked(blocker) => {
                        if blocker.nowait || non_blocking {
                            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                        }
                        if deadline_ticks.is_some_and(|deadline| clock() >= deadline) {
                            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                        }
                        let prepared_entry = plain_prepared_entry
                            .as_ref()
                            .expect("plain queued semop entry is preallocated");
                        set.update_blocker(prepared_entry, blocker);
                        set.enqueue_waiter(prepared_entry.clone());
                        break prepared_entry.clone();
                    }
                }
            }
        };

        drop(registry_spare);
        wakes.wake_all();
        if let Some(timer) = timer.as_ref() {
            timer.activate();
        }
        let _wait_result = waiter.wait(true);
        let completed = entry.completed_result();
        let was_timeout = timer.as_ref().is_some_and(|timer| timer.timeout());
        if !was_timeout {
            if let Some(timer) = timer.as_ref() {
                timer.cancel();
            }
        }
        if let Some(result) = completed {
            return result;
        }

        let error = if was_timeout {
            SystemError::EAGAIN_OR_EWOULDBLOCK
        } else {
            SystemError::EINTR
        };
        let mut guard = ipcns.sem.lock();
        guard.cancel_queued_entry(semid, &entry, error)
    }
}
