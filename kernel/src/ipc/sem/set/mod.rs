//! Set-owned values, metadata and undo associations. All mutation requires the manager lock.
use crate::{
    ipc::{
        ipc_perm::IpcPerm,
        sem_undo::{
            PreparedSemUndoRecord, PreparedSemUndoRecordAction, SemUndoGroup, SemUndoRecord,
        },
    },
    libs::{spinlock::SpinLock, wait_queue::Waker},
    process::{
        pid::{Pid, PidType},
        ProcessManager,
    },
    time::PosixTimeSpec,
};
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use super::abi::*;
mod operation;
mod queue;
mod undo_registry;
pub(in crate::ipc::sem) use operation::{SemAttempt, SemBlockedOp, SemWaitType, SemopScratch};
pub(in crate::ipc::sem) use queue::SemQueueEntry;
use queue::SemWaitQueue;
pub(crate) use queue::SemWakeBatch;
pub(in crate::ipc::sem) use undo_registry::SemUndoRegistry;

/// A single semaphore (fields of Linux `struct sem`)
#[derive(Debug, Clone)]
pub struct KernelSem {
    /// semval
    val: i32,
    /// sempid: process that last operated on this semaphore
    pid: Option<Arc<Pid>>,
    /// Counts of queued operations currently blocked on this semaphore.
    ncnt: usize,
    zcnt: usize,
}

/// A live undo registration, reused to own deferred cleanup after removal.
#[derive(Debug)]
pub(in crate::ipc::sem) enum SemUndoAssociation {
    Group(Weak<SemUndoGroup>),
    Retired {
        _group: Arc<SemUndoGroup>,
        _record: Option<SemUndoRecord>,
    },
}

impl SemUndoAssociation {
    /// Retired slots are only present in a removed, caller-owned set.
    fn group(&self) -> &Weak<SemUndoGroup> {
        match self {
            Self::Group(group) => group,
            Self::Retired { .. } => unreachable!("retired association in live set"),
        }
    }
}

/// Semaphore set
#[derive(Debug)]
pub struct KernelSemSet {
    /// Groups that can carry undo debt for this set, including queued operations.
    undo_groups: SemUndoRegistry,
    /// Permission information
    kern_ipc_perm: IpcPerm,
    /// Semaphores in the set
    sems: Vec<KernelSem>,
    /// Time of the last `semop`
    sem_otime: i64,
    /// Time of the last metadata change
    sem_ctime: i64,
    /// Pending operation groups containing only zero-wait operations
    pending_const: SemWaitQueue,
    /// Pending operation groups containing at least one altering operation
    pending_alter: SemWaitQueue,
}

impl KernelSemSet {
    /// Only call after RMID released the manager lock and delivered wakeups.
    pub(crate) fn reclaim_removed_undo_storage(&self) {
        for association in self.undo_groups.iter() {
            if let SemUndoAssociation::Retired { _group: group, .. } = association {
                group.shrink_records();
            }
        }
    }

    /// Live associations survive SETVAL/SETALL. Only RMID or dead groups
    /// remove them, so an existing undo record proves prior association.
    /// Insufficient spare capacity requests an unlocked preparation/retry.
    /// On success, spare retains any replaced allocation for unlocked disposal.
    pub(in crate::ipc::sem) fn ensure_undo_group_registered_prepared(
        &mut self,
        group: &Arc<SemUndoGroup>,
        spare: &mut SemUndoRegistry,
    ) -> Result<(), usize> {
        self.undo_groups.register_prepared(group, spare)
    }

    fn compact_undo_registry(&mut self) {
        self.undo_groups.compact();
    }

    pub(in crate::ipc::sem) fn shrink_undo_registry_prepared(
        &mut self,
        spare: &mut SemUndoRegistry,
        retired: &mut SemUndoRegistry,
    ) -> usize {
        self.undo_groups.shrink_prepared(spare, retired)
    }

    pub(in crate::ipc::sem) fn try_allocate_sems(
        nsems: usize,
    ) -> Result<Vec<KernelSem>, SystemError> {
        let mut sems = Vec::new();
        sems.try_reserve_exact(nsems)
            .map_err(|_| SystemError::ENOMEM)?;
        sems.resize(
            nsems,
            KernelSem {
                val: 0,
                pid: None,
                ncnt: 0,
                zcnt: 0,
            },
        );
        Ok(sems)
    }

    pub(in crate::ipc::sem) fn new(kern_ipc_perm: IpcPerm, sems: Vec<KernelSem>) -> Self {
        KernelSemSet {
            undo_groups: SemUndoRegistry::default(),
            kern_ipc_perm,
            sems,
            sem_otime: 0,
            sem_ctime: PosixTimeSpec::now().tv_sec,
            pending_const: SemWaitQueue::default(),
            pending_alter: SemWaitQueue::default(),
        }
    }
}

impl KernelSemSet {
    pub(in crate::ipc::sem) fn permissions(&self) -> &IpcPerm {
        &self.kern_ipc_perm
    }
    pub(in crate::ipc::sem) fn nsems(&self) -> usize {
        self.sems.len()
    }
    pub(in crate::ipc::sem) fn stat(
        &self,
        sem_perm: crate::ipc::ipc_perm::PosixIpcPerm,
    ) -> PosixSemIdDs {
        PosixSemIdDs::new(sem_perm, self.sem_otime, self.sem_ctime, self.nsems())
    }
    pub(in crate::ipc::sem) fn set_permissions(
        &mut self,
        data: PosixSemIdDs,
    ) -> Result<(), SystemError> {
        self.kern_ipc_perm.copy_from_posix(
            data.sem_perm.uid(),
            data.sem_perm.gid(),
            data.sem_perm.mode(),
            &ProcessManager::current_user_ns(),
        )?;
        self.sem_ctime = PosixTimeSpec::now().tv_sec;
        Ok(())
    }
    pub(in crate::ipc::sem) fn get_value(
        &self,
        semnum: usize,
        cmd: SemCtlCmd,
    ) -> Result<usize, SystemError> {
        match cmd {
            SemCtlCmd::GetVal => Ok(self.sems[semnum].val as usize),
            SemCtlCmd::GetPid => Ok(self.sems[semnum]
                .pid
                .as_ref()
                .map(|pid| pid.pid_vnr().data())
                .unwrap_or(0)),
            SemCtlCmd::GetNcnt => Ok(self.sems[semnum].ncnt),
            SemCtlCmd::GetZcnt => Ok(self.sems[semnum].zcnt),
            _ => Err(SystemError::EINVAL),
        }
    }
    pub(in crate::ipc::sem) fn values(&self) -> Result<Vec<u16>, SystemError> {
        let mut vals = Vec::new();
        vals.try_reserve_exact(self.nsems())
            .map_err(|_| SystemError::ENOMEM)?;
        vals.extend(self.sems.iter().map(|s| s.val as u16));
        Ok(vals)
    }
    pub(in crate::ipc::sem) fn unregister_undo_group(&mut self, group: &Arc<SemUndoGroup>) {
        self.undo_groups.unregister(group);
    }
    pub(in crate::ipc::sem) fn retire_undo_records(&mut self, id: SemId) {
        // Removed set owns all disposal until the caller releases the manager lock.
        for association in self.undo_groups.iter_mut() {
            if let Some(group) = association.group().upgrade() {
                let record = group.take_record(id);
                *association = SemUndoAssociation::Retired {
                    _group: group,
                    _record: record,
                };
            }
        }
    }
    pub(in crate::ipc::sem) fn clear_undo_for_setval(&mut self, semid: SemId, semnum: usize) {
        let mut saw_stale = false;
        for weak in self.undo_groups.iter().map(SemUndoAssociation::group) {
            let Some(group) = weak.upgrade() else {
                saw_stale = true;
                continue;
            };
            group.with_record_mut(semid, |record| {
                if semnum < record.adjustment_count() {
                    record.clear_adjustment(semnum);
                }
            });
        }
        if saw_stale {
            self.compact_undo_registry();
        }
    }
    pub(in crate::ipc::sem) fn clear_undo_for_setall(&mut self, semid: SemId) {
        let mut saw_stale = false;
        for weak in self.undo_groups.iter().map(SemUndoAssociation::group) {
            let Some(group) = weak.upgrade() else {
                saw_stale = true;
                continue;
            };
            group.with_record_mut(semid, |record| record.clear_all_adjustments());
        }
        if saw_stale {
            self.compact_undo_registry();
        }
    }
    pub(in crate::ipc::sem) fn setval(
        &mut self,
        id: SemId,
        semnum: usize,
        val: i32,
        wakes: &mut SemWakeBatch,
    ) {
        self.clear_undo_for_setval(id, semnum);
        let sem = &mut self.sems[semnum];
        sem.val = val;
        sem.pid = ProcessManager::current_pcb().task_pid_ptr(PidType::TGID);
        self.sem_ctime = PosixTimeSpec::now().tv_sec;
        self.update_queue(wakes);
    }
    pub(in crate::ipc::sem) fn setall(
        &mut self,
        id: SemId,
        vals: &[u16],
        wakes: &mut SemWakeBatch,
    ) {
        self.clear_undo_for_setall(id);
        let pid = ProcessManager::current_pcb().task_pid_ptr(PidType::TGID);
        for (i, &v) in vals.iter().enumerate() {
            let sem = &mut self.sems[i];
            sem.val = v as i32;
            sem.pid = pid.clone();
        }
        self.sem_ctime = PosixTimeSpec::now().tv_sec;
        self.update_queue(wakes);
    }
    pub(in crate::ipc::sem) fn replay_undo(
        &mut self,
        adjustments: &[i16],
        exiting_tgid: Option<Arc<Pid>>,
        wakes: &mut SemWakeBatch,
    ) {
        for (sem, adjustment) in self.sems.iter_mut().zip(adjustments.iter().copied()) {
            if adjustment == 0 {
                continue;
            }
            sem.val = (sem.val as i64 + adjustment as i64).clamp(0, SEMVMX as i64) as i32;
            sem.pid = exiting_tgid.clone();
        }
        self.sem_otime = PosixTimeSpec::now().tv_sec;
        self.update_queue(wakes);
    }
}

#[cfg(test)]
pub(in crate::ipc::sem) mod test_support;
#[cfg(test)]
mod tests;
