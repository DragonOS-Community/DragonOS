//! Namespace identity, lookup, quota and semaphore creation.
use super::{
    abi::*,
    set::{KernelSem, KernelSemSet, SemUndoRegistry, SemWakeBatch},
};
use crate::{
    ipc::{
        id::IpcIdAllocator,
        ipc_perm::{self, IpcPerm},
        sem_undo::SemUndoGroup,
    },
    process::{namespace::ipc_namespace::IpcNamespace, pid::Pid, ProcessManager},
};
use alloc::{sync::Arc, vec::Vec};
use hashbrown::HashMap;
use system_error::SystemError;
mod control;
pub use control::SemSetAllToken;

/// Semaphore manager
#[derive(Debug)]
pub struct SemManager {
    /// SemId allocator
    id_allocator: IpcIdAllocator,
    /// Semaphore set table keyed by low IPC index
    id2sem: HashMap<usize, KernelSemSet>,
    /// SemId table keyed by SemKey
    key2id: HashMap<SemKey, SemId>,
    /// Total semaphores in the namespace (Linux semmns accounting)
    total_sems: usize,
}

impl Default for SemManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SemManager {
    pub(super) const IPC_READ: u32 = 0o4;
    pub(super) const IPC_WRITE: u32 = 0o2;

    pub fn new() -> Self {
        SemManager {
            id_allocator: IpcIdAllocator::new(SEMMNI).unwrap(),
            id2sem: HashMap::new(),
            key2id: HashMap::new(),
            total_sems: 0,
        }
    }

    pub(super) fn get_by_semid_checked(&self, id: SemId) -> Result<&KernelSemSet, SystemError> {
        let decoded = IpcIdAllocator::decode(id.data())?;
        let set = self.id2sem.get(&decoded.idx).ok_or(SystemError::EINVAL)?;
        if set.permissions().id != id.data() || set.permissions().seq != decoded.seq {
            return Err(SystemError::EINVAL);
        }
        Ok(set)
    }

    pub(super) fn get_by_semid_checked_mut(
        &mut self,
        id: SemId,
    ) -> Result<&mut KernelSemSet, SystemError> {
        let decoded = IpcIdAllocator::decode(id.data())?;
        let set = self
            .id2sem
            .get_mut(&decoded.idx)
            .ok_or(SystemError::EINVAL)?;
        if set.permissions().id != id.data() || set.permissions().seq != decoded.seq {
            return Err(SystemError::EINVAL);
        }
        Ok(set)
    }

    fn get_by_index(&self, id: usize) -> Result<&KernelSemSet, SystemError> {
        let idx = id & IpcIdAllocator::IPC_ID_IDX_MASK;
        self.id2sem.get(&idx).ok_or(SystemError::EINVAL)
    }

    pub(super) fn validate_semid_nsems(&self, semid: SemId) -> Result<usize, SystemError> {
        Ok(self.get_by_semid_checked(semid)?.nsems())
    }

    /// Reclaim a target registry, never allocating or freeing its buffer under
    /// the namespace lock. Concurrent growth can make a shrink unnecessary or
    /// consume the spare capacity; in either case leave the live registry intact.
    pub(crate) fn shrink_undo_registry(ipcns: &Arc<IpcNamespace>, semid: SemId) {
        let mut spare = SemUndoRegistry::default();
        let mut retired = SemUndoRegistry::default();
        let needed = {
            let mut manager = ipcns.sem.lock();
            let Ok(set) = manager.get_by_semid_checked_mut(semid) else {
                return;
            };
            set.shrink_undo_registry_prepared(&mut spare, &mut retired)
        };
        if needed == 0 || spare.prepare(needed).is_err() {
            return;
        }
        // Allocation is best-effort reclamation, not part of syscall success.
        let mut manager = ipcns.sem.lock();
        if let Ok(set) = manager.get_by_semid_checked_mut(semid) {
            set.shrink_undo_registry_prepared(&mut spare, &mut retired);
        }
    }

    fn current_max_index(&self) -> usize {
        self.id_allocator.max_used_index().unwrap_or(0)
    }

    /// Create or look up a set. Storage preparation and disposal must happen
    /// outside the namespace spinlock, including when another creator wins.
    pub fn semget(
        ipcns: &Arc<IpcNamespace>,
        key: SemKey,
        nsems: usize,
        semflg: SemFlags,
    ) -> Result<usize, SystemError> {
        let mut sems = Vec::new();
        let mut id_spare = HashMap::new();
        let mut key_spare = HashMap::new();
        loop {
            let mut manager = ipcns.sem.lock();
            if let Some(id) = manager.lookup_semget(key, nsems, semflg)? {
                return Ok(id);
            }
            if sems.is_empty() {
                drop(manager);
                sems = KernelSemSet::try_allocate_sems(nsems)?;
                continue;
            }
            if let Err((id_capacity, key_capacity)) =
                manager.install_create_tables(key, &mut id_spare, &mut key_spare)
            {
                drop(manager);
                id_spare
                    .try_reserve(id_capacity)
                    .map_err(|_| SystemError::ENOMEM)?;
                key_spare
                    .try_reserve(key_capacity)
                    .map_err(|_| SystemError::ENOMEM)?;
                continue;
            }
            return manager.create_prepared(key, semflg, &mut sems);
        }
    }

    /// None means creation is currently permitted, not a reservation. Call
    /// again after unlocked preparation to recheck key races and quotas.
    fn lookup_semget(
        &self,
        key: SemKey,
        nsems: usize,
        semflg: SemFlags,
    ) -> Result<Option<usize>, SystemError> {
        if nsems > SEMMSL {
            return Err(SystemError::EINVAL);
        }

        if key == IPC_PRIVATE {
            self.validate_create(nsems)?;
            return Ok(None);
        }

        if let Some(&id) = self.key2id.get(&key) {
            if semflg.contains(SemFlags::IPC_CREAT | SemFlags::IPC_EXCL) {
                return Err(SystemError::EEXIST);
            }
            let set = self.get_by_semid_checked(id)?;
            if nsems > set.nsems() {
                return Err(SystemError::EINVAL);
            }
            let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
            ipc_perm::ipc_permission(
                set.permissions(),
                semflg.bits() & SemFlags::PERM_MASK.bits(),
                &target_user_ns,
            )?;
            return Ok(Some(id.data()));
        }

        if !semflg.contains(SemFlags::IPC_CREAT) {
            return Err(SystemError::ENOENT);
        }
        self.validate_create(nsems)?;
        Ok(None)
    }

    fn validate_create(&self, nsems: usize) -> Result<usize, SystemError> {
        if nsems == 0 {
            return Err(SystemError::EINVAL);
        }
        if self.id2sem.len() >= SEMMNI {
            return Err(SystemError::ENOSPC);
        }
        let total_after = self
            .total_sems
            .checked_add(nsems)
            .ok_or(SystemError::ENOSPC)?;
        if total_after > SEMMNS {
            return Err(SystemError::ENOSPC);
        }
        Ok(total_after)
    }

    /// Install preallocated tables only when BOTH have enough room. Rehashing
    /// still takes O(n) under the lock, but never allocates. The emptied old
    /// tables remain in the caller's spares for disposal after unlocking.
    fn install_create_tables(
        &mut self,
        key: SemKey,
        id_spare: &mut HashMap<usize, KernelSemSet>,
        key_spare: &mut HashMap<SemKey, SemId>,
    ) -> Result<(), (usize, usize)> {
        debug_assert!(id_spare.is_empty() && key_spare.is_empty());
        let grow_ids = self.id2sem.capacity() == self.id2sem.len();
        let grow_keys = key != IPC_PRIVATE && self.key2id.capacity() == self.key2id.len();
        let needed_ids = if grow_ids && id_spare.capacity() <= self.id2sem.len() {
            self.id2sem.len().saturating_mul(2).max(4)
        } else {
            0
        };
        let needed_keys = if grow_keys && key_spare.capacity() <= self.key2id.len() {
            self.key2id.len().saturating_mul(2).max(4)
        } else {
            0
        };
        if needed_ids != 0 || needed_keys != 0 {
            return Err((needed_ids, needed_keys));
        }
        if grow_ids {
            core::mem::swap(&mut self.id2sem, id_spare);
            for (id, set) in id_spare.drain() {
                self.id2sem.insert(id, set);
            }
        }
        if grow_keys {
            core::mem::swap(&mut self.key2id, key_spare);
            for (key, id) in key_spare.drain() {
                self.key2id.insert(key, id);
            }
        }
        Ok(())
    }

    fn create_prepared(
        &mut self,
        key: SemKey,
        semflg: SemFlags,
        sems: &mut Vec<KernelSem>,
    ) -> Result<usize, SystemError> {
        let total_after = self.validate_create(sems.len())?;
        debug_assert!(self.id2sem.capacity() > self.id2sem.len());
        debug_assert!(key == IPC_PRIVATE || self.key2id.capacity() > self.key2id.len());
        let ipc_id = self.id_allocator.alloc()?;
        let sem_id = SemId::new(ipc_id.raw);
        let current_cred = ProcessManager::current_pcb().cred();
        let kern_ipc_perm = IpcPerm::new_with_cred(
            sem_id.data(),
            key.data(),
            current_cred,
            semflg.bits() & SemFlags::PERM_MASK.bits(),
            ipc_id.seq,
        );
        let set = KernelSemSet::new(kern_ipc_perm, core::mem::take(sems));

        if key != IPC_PRIVATE {
            self.key2id.insert(key, sem_id);
        }
        self.id2sem.insert(ipc_id.idx, set);
        self.total_sems = total_after;

        Ok(sem_id.data())
    }

    #[cfg(test)]
    pub(crate) fn semget_for_test(
        &mut self,
        key: SemKey,
        nsems: usize,
        flags: SemFlags,
    ) -> Result<usize, SystemError> {
        if let Some(id) = self.lookup_semget(key, nsems, flags)? {
            return Ok(id);
        }
        let mut sems = KernelSemSet::try_allocate_sems(nsems)?;
        let mut ids = HashMap::new();
        let mut keys = HashMap::new();
        if let Err((id_capacity, key_capacity)) =
            self.install_create_tables(key, &mut ids, &mut keys)
        {
            ids.try_reserve(id_capacity)
                .map_err(|_| SystemError::ENOMEM)?;
            keys.try_reserve(key_capacity)
                .map_err(|_| SystemError::ENOMEM)?;
            self.install_create_tables(key, &mut ids, &mut keys)
                .unwrap();
        }
        self.create_prepared(key, flags, &mut sems)
    }
}

impl SemManager {
    pub(crate) fn unregister_undo_group(&mut self, semid: SemId, group: &Arc<SemUndoGroup>) {
        if let Ok(set) = self.get_by_semid_checked_mut(semid) {
            set.unregister_undo_group(group);
        }
    }
    pub(crate) fn replay_sem_undo_adjustments(
        &mut self,
        semid: SemId,
        adjustments: &[i16],
        exiting_tgid: Option<Arc<Pid>>,
        wakes: &mut SemWakeBatch,
    ) {
        if let Ok(set) = self.get_by_semid_checked_mut(semid) {
            set.replay_undo(adjustments, exiting_tgid, wakes);
        }
    }
}

#[cfg(test)]
pub(in crate::ipc::sem) mod test_support;
#[cfg(test)]
mod tests;
