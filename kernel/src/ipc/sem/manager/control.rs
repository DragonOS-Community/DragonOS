//! Command-specific validation and dispatch; user copies remain in syscall wrappers.
use super::*;
use crate::ipc::ipc_perm::IpcPermView;

pub struct SemSetAllToken {
    id: SemId,
    nsems: usize,
}

impl SemSetAllToken {
    pub(super) fn new(id: SemId, nsems: usize) -> Self {
        Self { id, nsems }
    }

    pub fn nsems(&self) -> usize {
        self.nsems
    }
}

impl SemManager {
    /// # IPC_RMID: remove the semaphore set and wake all waiters with EIDRM
    /// The caller must release the manager guard before notifying `wakes` and
    /// dropping the returned set, which owns all deferred undo/group disposal.
    pub(crate) fn ipc_rmid(
        &mut self,
        id: SemId,
        wakes: &mut SemWakeBatch,
    ) -> Result<KernelSemSet, SystemError> {
        let decoded = IpcIdAllocator::decode(id.data())?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        let key = {
            let set = self.get_by_semid_checked(id)?;
            ipc_perm::check_control_permission(set.permissions(), &target_user_ns)?;
            set.permissions().key
        };
        let mut set = self
            .id2sem
            .remove(&decoded.idx)
            .ok_or(SystemError::EINVAL)?;
        self.key2id.remove(&SemKey::new(key));
        self.total_sems = self.total_sems.saturating_sub(set.nsems());
        // Reuse existing association storage: neither allocation nor undo/group
        // destruction is allowed here. Even an empty upgraded group stays alive
        // until the caller drops this removed set outside the manager lock.
        set.retire_undo_records(id);
        set.complete_all_removed(wakes);
        self.id_allocator.free_idx(decoded.idx);
        Ok(set)
    }

    /// # IPC_SET: update permissions (uid/gid/mode) and refresh `sem_ctime`
    pub fn ipc_set(&mut self, id: SemId, semid_ds: PosixSemIdDs) -> Result<(), SystemError> {
        let set = self.get_by_semid_checked_mut(id)?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        ipc_perm::check_control_permission(set.permissions(), &target_user_ns)?;
        set.set_permissions(semid_ds)
    }

    /// IPC_STAT/SEM_STAT/SEM_STAT_ANY: return `semid_ds`
    pub fn sem_stat_data(
        &self,
        id_or_index: SemId,
        cmd: SemCtlCmd,
    ) -> Result<(usize, PosixSemIdDs), SystemError> {
        let set = match cmd {
            SemCtlCmd::IpcStat => self.get_by_semid_checked(id_or_index)?,
            SemCtlCmd::SemStat | SemCtlCmd::SemStatAny => self.get_by_index(id_or_index.data())?,
            _ => return Err(SystemError::EINVAL),
        };
        if cmd != SemCtlCmd::SemStatAny {
            let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
            ipc_perm::ipc_permission(set.permissions(), Self::IPC_READ, &target_user_ns)?;
        }
        let current_user_ns = ProcessManager::current_user_ns();
        let sem_perm = set.permissions().to_posix(&current_user_ns)?;
        let semid_ds = set.stat(sem_perm);
        let ret = if cmd == SemCtlCmd::IpcStat {
            0
        } else {
            set.permissions().id
        };
        Ok((ret, semid_ds))
    }

    /// IPC_INFO/SEM_INFO: return system information
    pub fn sem_info_data(&self, cmd: SemCtlCmd) -> (usize, PosixSemInfo) {
        (
            self.current_max_index(),
            PosixSemInfo::new(cmd, self.id2sem.len(), self.total_sems),
        )
    }

    /// GETVAL/GETPID/GETNCNT/GETZCNT: query a single semaphore
    pub fn sem_get_value(
        &self,
        id: SemId,
        semnum: usize,
        cmd: SemCtlCmd,
    ) -> Result<usize, SystemError> {
        let set = self.get_by_semid_checked(id)?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        ipc_perm::ipc_permission(set.permissions(), Self::IPC_READ, &target_user_ns)?;
        if semnum >= set.nsems() {
            return Err(SystemError::EINVAL);
        }
        set.get_value(semnum, cmd)
    }

    /// # SETVAL: set a single semaphore value
    pub(crate) fn setval(
        &mut self,
        id: SemId,
        semnum: usize,
        val: i32,
        wakes: &mut SemWakeBatch,
    ) -> Result<(), SystemError> {
        // Match Linux: validate the value (ERANGE), then semnum (EINVAL), then permissions
        // (EACCES).
        if !(0..=SEMVMX).contains(&val) {
            return Err(SystemError::ERANGE);
        }
        let nsems = {
            let set = self.get_by_semid_checked(id)?;
            if semnum >= set.nsems() {
                return Err(SystemError::EINVAL);
            }
            let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
            ipc_perm::ipc_permission(set.permissions(), Self::IPC_WRITE, &target_user_ns)?;
            set.nsems()
        };
        debug_assert!(semnum < nsems);

        let set = self.get_by_semid_checked_mut(id)?;
        set.setval(id, semnum, val, wakes);
        Ok(())
    }

    /// # SETALL: set values of all semaphores in the set without changes on validation failure
    pub(crate) fn setall(
        &mut self,
        token: SemSetAllToken,
        vals: &[u16],
        wakes: &mut SemWakeBatch,
    ) -> Result<(), SystemError> {
        // Linux validates the copied array before re-locking the target:
        // an out-of-range value wins over removal during the user copy.
        if vals.len() != token.nsems {
            return Err(SystemError::EINVAL);
        }
        if vals.iter().any(|&v| v as i32 > SEMVMX) {
            return Err(SystemError::ERANGE);
        }

        let set = self
            .get_by_semid_checked_mut(token.id)
            .map_err(|_| SystemError::EIDRM)?;
        if vals.len() != set.nsems() {
            return Err(SystemError::EINVAL);
        }
        set.setall(token.id, vals, wakes);
        Ok(())
    }

    /// # GETALL: get values of all semaphores in the set
    pub fn getall(&self, id: SemId) -> Result<Vec<u16>, SystemError> {
        let set = self.get_by_semid_checked(id)?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        ipc_perm::ipc_permission(set.permissions(), Self::IPC_READ, &target_user_ns)?;
        set.values()
    }

    /// Validate SETALL before the caller accesses the userspace array.
    pub fn prepare_setall(&self, id: SemId) -> Result<SemSetAllToken, SystemError> {
        let set = self.get_by_semid_checked(id)?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        ipc_perm::ipc_permission(set.permissions(), Self::IPC_WRITE, &target_user_ns)?;
        Ok(SemSetAllToken::new(id, set.nsems()))
    }
}
