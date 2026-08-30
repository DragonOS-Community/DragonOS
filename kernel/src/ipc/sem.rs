// SPDX-License-Identifier: GPL-2.0-or-later
//
// System V semaphore subsystem, tracking Linux 6.6 `ipc/sem.c` observable
// behavior. SEM_UNDO is intentionally unsupported and rejected with ENOSYS.

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::fmt;

use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    ipc::{
        id::IpcIdAllocator,
        ipc_perm::{self, IpcPerm, IpcPermView, PosixIpcPerm},
    },
    libs::{
        spinlock::SpinLock,
        wait_queue::{TimeoutWaker, Waiter, Waker},
    },
    process::{
        namespace::ipc_namespace::IpcNamespace,
        pid::{Pid, PidType},
        ProcessManager,
    },
    time::{
        timer::{clock, next_n_us_timer_jiffies, Timer},
        Duration, PosixTimeSpec,
    },
};

/// Used to create a new private semaphore set
pub const IPC_PRIVATE: SemKey = SemKey::new(0);

int_like!(SemId, usize);
int_like!(SemKey, usize);

#[derive(Debug, Clone, Copy)]
pub struct SemSetAllToken {
    id: SemId,
    nsems: usize,
}

impl SemSetAllToken {
    fn new(id: SemId, nsems: usize) -> Self {
        Self { id, nsems }
    }

    pub fn nsems(&self) -> usize {
        self.nsems
    }
}

// Limit constants from Linux include/uapi/linux/sem.h
pub const SEMMNI: usize = 32000;
pub const SEMMSL: usize = 32000;
pub const SEMMNS: usize = SEMMNI * SEMMSL;
pub const SEMOPM: usize = 500;
pub const SEMVMX: i32 = 32767;

bitflags! {
    pub struct SemFlags: u32 {
        const PERM_MASK = 0o777;
        const IPC_CREAT = 0o1000;
        const IPC_EXCL = 0o2000;
        const IPC_NOWAIT = 0x800;
        const SEM_UNDO = 0x1000;
    }
}

/// Semaphore-set control commands (Linux x86_64 UAPI include/uapi/linux/sem.h)
#[derive(Eq, Clone, Copy)]
pub enum SemCtlCmd {
    /// Remove the semaphore set
    IpcRmid = 0,
    /// Set permissions
    IpcSet = 1,
    /// Retrieve `SemIdDs`
    IpcStat = 2,
    /// Retrieve `SemInfo`
    IpcInfo = 3,
    /// Get the PID of the last process to operate on the specified semaphore
    GetPid = 11,
    /// Get the specified semaphore value
    GetVal = 12,
    /// Get values of all semaphores in the set
    GetAll = 13,
    /// Get the number of processes waiting for the specified semaphore to increase
    GetNcnt = 14,
    /// Get the number of processes waiting for the specified semaphore to reach zero
    GetZcnt = 15,
    /// Set the specified semaphore value
    SetVal = 16,
    /// Set values of all semaphores in the set
    SetAll = 17,
    /// Retrieve `SemIdDs` by index
    SemStat = 18,
    /// Retrieve `SemInfo`
    SemInfo = 19,
    /// Retrieve `SemIdDs` by index without permission checks
    SemStatAny = 20,

    Default,
}

impl From<usize> for SemCtlCmd {
    fn from(cmd: usize) -> SemCtlCmd {
        match cmd {
            0 => Self::IpcRmid,
            1 => Self::IpcSet,
            2 => Self::IpcStat,
            3 => Self::IpcInfo,
            11 => Self::GetPid,
            12 => Self::GetVal,
            13 => Self::GetAll,
            14 => Self::GetNcnt,
            15 => Self::GetZcnt,
            16 => Self::SetVal,
            17 => Self::SetAll,
            18 => Self::SemStat,
            19 => Self::SemInfo,
            20 => Self::SemStatAny,
            _ => Self::Default,
        }
    }
}

impl fmt::Display for SemCtlCmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SemCtlCmd::IpcRmid => write!(f, "IPC_RMID"),
            SemCtlCmd::IpcSet => write!(f, "IPC_SET"),
            SemCtlCmd::IpcStat => write!(f, "IPC_STAT"),
            SemCtlCmd::IpcInfo => write!(f, "IPC_INFO"),
            SemCtlCmd::GetPid => write!(f, "GETPID"),
            SemCtlCmd::GetVal => write!(f, "GETVAL"),
            SemCtlCmd::GetAll => write!(f, "GETALL"),
            SemCtlCmd::GetNcnt => write!(f, "GETNCNT"),
            SemCtlCmd::GetZcnt => write!(f, "GETZCNT"),
            SemCtlCmd::SetVal => write!(f, "SETVAL"),
            SemCtlCmd::SetAll => write!(f, "SETALL"),
            SemCtlCmd::SemStat => write!(f, "SEM_STAT"),
            SemCtlCmd::SemInfo => write!(f, "SEM_INFO"),
            SemCtlCmd::SemStatAny => write!(f, "SEM_STAT_ANY"),
            SemCtlCmd::Default => write!(f, "DEFAULT (Invalid Cmd)"),
        }
    }
}

impl PartialEq for SemCtlCmd {
    fn eq(&self, other: &SemCtlCmd) -> bool {
        *self as usize == *other as usize
    }
}

/// A single semaphore (fields of Linux `struct sem`)
#[derive(Debug, Clone)]
pub struct KernelSem {
    /// semval
    val: i32,
    /// sempid: process that last operated on this semaphore
    pid: Option<Arc<Pid>>,
}

/// Userspace `sembuf` (Linux `struct sembuf`, 6 bytes)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PosixSemBuf {
    pub sem_num: u16,
    pub sem_op: i16,
    pub sem_flg: i16,
}

/// Semaphore-set information matching Linux x86_64 `struct semid64_ds` (104 bytes,
/// including the high halves of 32-bit timestamp fields)
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PosixSemIdDs {
    /// Permission information
    pub sem_perm: PosixIpcPerm,
    /// Time of the last `semop` (64-bit; upper 32 bits are in `__sem_otime_high`)
    pub sem_otime: i64,
    _sem_otime_high: i64,
    /// Time of the last metadata change
    pub sem_ctime: i64,
    _sem_ctime_high: i64,
    /// Number of semaphores in the set
    pub sem_nsems: usize,
    _unused1: usize,
    _unused2: usize,
}

/// Semaphore system information matching Linux `struct seminfo` (40 bytes)
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PosixSemInfo {
    pub semmap: i32,
    pub semmni: i32,
    pub semmns: i32,
    pub semmnu: i32,
    pub semmsl: i32,
    pub semopm: i32,
    pub semume: i32,
    pub semusz: i32,
    pub semvmx: i32,
    pub semaem: i32,
}

impl PosixSemInfo {
    fn new(cmd: SemCtlCmd, set_count: usize, total_sems: usize) -> Self {
        let (semusz, semaem) = if cmd == SemCtlCmd::SemInfo {
            (set_count as i32, total_sems as i32)
        } else {
            (20, SEMVMX)
        };
        PosixSemInfo {
            semmap: SEMMNS as i32,
            semmni: SEMMNI as i32,
            semmns: SEMMNS as i32,
            semmnu: SEMMNS as i32,
            semmsl: SEMMSL as i32,
            semopm: SEMOPM as i32,
            semume: SEMOPM as i32,
            semusz,
            semvmx: SEMVMX,
            semaem,
        }
    }
}

/// Why a waiter is blocked, determining wakeup timing and GETNCNT/GETZCNT accounting
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SemWaitType {
    /// `sem_op < 0`: wait for semval to increase (GETNCNT)
    Increase,
    /// `sem_op == 0`: wait for semval to reach zero (GETZCNT)
    Zero,
}

/// The precise operation currently blocking an operation group.
#[derive(Debug, Clone, Copy)]
struct SemBlockedOp {
    semnum: usize,
    wait_type: SemWaitType,
    nowait: bool,
}

#[derive(Debug)]
enum SemQueueStatus {
    Queued(SemBlockedOp),
    Completed(Result<usize, SystemError>),
}

/// An Arc-owned queue entry shared by the set and the blocked caller.
#[derive(Debug)]
struct SemQueueEntry {
    sops: Vec<PosixSemBuf>,
    pid: Option<Arc<Pid>>,
    waker: Arc<Waker>,
    status: SpinLock<SemQueueStatus>,
}

impl SemQueueEntry {
    fn new(
        sops: &[PosixSemBuf],
        pid: Option<Arc<Pid>>,
        waker: Arc<Waker>,
        blocker: SemBlockedOp,
    ) -> Self {
        Self {
            sops: sops.to_vec(),
            pid,
            waker,
            status: SpinLock::new(SemQueueStatus::Queued(blocker)),
        }
    }

    fn completed_result(&self) -> Option<Result<usize, SystemError>> {
        match &*self.status.lock() {
            SemQueueStatus::Queued(_) => None,
            SemQueueStatus::Completed(result) => Some(result.clone()),
        }
    }

    fn update_blocker(&self, blocker: SemBlockedOp) {
        let mut status = self.status.lock();
        if matches!(&*status, SemQueueStatus::Queued(_)) {
            *status = SemQueueStatus::Queued(blocker);
        }
    }

    fn complete(&self, result: Result<usize, SystemError>) -> bool {
        let mut status = self.status.lock();
        if matches!(&*status, SemQueueStatus::Completed(_)) {
            return false;
        }
        *status = SemQueueStatus::Completed(result);
        true
    }

    fn is_waiting_on(&self, semnum: usize, wait_type: SemWaitType) -> bool {
        matches!(
            &*self.status.lock(),
            SemQueueStatus::Queued(blocker)
                if blocker.semnum == semnum && blocker.wait_type == wait_type
        )
    }
}

/// Semaphore set
#[derive(Debug)]
pub struct KernelSemSet {
    /// Permission information
    pub kern_ipc_perm: IpcPerm,
    /// Semaphores in the set
    pub sems: Vec<KernelSem>,
    /// Time of the last `semop`
    pub sem_otime: i64,
    /// Time of the last metadata change
    pub sem_ctime: i64,
    /// Waiter queue
    waiters: VecDeque<Arc<SemQueueEntry>>,
}

impl KernelSemSet {
    fn try_allocate_sems(nsems: usize) -> Result<Vec<KernelSem>, SystemError> {
        let mut sems = Vec::new();
        sems.try_reserve_exact(nsems)
            .map_err(|_| SystemError::ENOMEM)?;
        sems.resize(nsems, KernelSem { val: 0, pid: None });
        Ok(sems)
    }

    fn new(kern_ipc_perm: IpcPerm, sems: Vec<KernelSem>) -> Self {
        KernelSemSet {
            kern_ipc_perm,
            sems,
            sem_otime: 0,
            sem_ctime: PosixTimeSpec::now().tv_sec,
            waiters: VecDeque::new(),
        }
    }

    fn enqueue_waiter(&mut self, waiter: Arc<SemQueueEntry>) {
        self.waiters.push_back(waiter);
    }

    fn remove_waiter(&mut self, target: &Arc<SemQueueEntry>) {
        self.waiters.retain(|entry| !Arc::ptr_eq(entry, target));
    }

    /// Complete and wake all entries during IPC_RMID under the manager lock.
    fn complete_all_removed(&mut self) {
        for entry in self.waiters.drain(..) {
            entry.complete(Err(SystemError::EIDRM));
            entry.waker.wake();
        }
    }

    fn ncnt(&self, semnum: usize) -> usize {
        self.waiters
            .iter()
            .filter(|entry| entry.is_waiting_on(semnum, SemWaitType::Increase))
            .count()
    }

    fn zcnt(&self, semnum: usize) -> usize {
        self.waiters
            .iter()
            .filter(|entry| entry.is_waiting_on(semnum, SemWaitType::Zero))
            .count()
    }
}

#[derive(Debug)]
struct SemopSimulation {
    values: HashMap<usize, i32>,
}

/// Result of an attempted `semop` execution
#[derive(Debug)]
enum SemopOutcome {
    Ready(SemopSimulation),
    Blocked(SemBlockedOp),
}

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
    const IPC_READ: u32 = 0o4;
    const IPC_WRITE: u32 = 0o2;

    pub fn new() -> Self {
        SemManager {
            id_allocator: IpcIdAllocator::new(SEMMNI).unwrap(),
            id2sem: HashMap::new(),
            key2id: HashMap::new(),
            total_sems: 0,
        }
    }

    fn get_by_semid_checked(&self, id: SemId) -> Result<&KernelSemSet, SystemError> {
        let decoded = IpcIdAllocator::decode(id.data())?;
        let set = self.id2sem.get(&decoded.idx).ok_or(SystemError::EINVAL)?;
        if set.kern_ipc_perm.id != id.data() || set.kern_ipc_perm.seq != decoded.seq {
            return Err(SystemError::EINVAL);
        }
        Ok(set)
    }

    fn get_by_semid_checked_mut(&mut self, id: SemId) -> Result<&mut KernelSemSet, SystemError> {
        let decoded = IpcIdAllocator::decode(id.data())?;
        let set = self
            .id2sem
            .get_mut(&decoded.idx)
            .ok_or(SystemError::EINVAL)?;
        if set.kern_ipc_perm.id != id.data() || set.kern_ipc_perm.seq != decoded.seq {
            return Err(SystemError::EINVAL);
        }
        Ok(set)
    }

    fn get_by_index(&self, id: usize) -> Result<&KernelSemSet, SystemError> {
        let idx = id & IpcIdAllocator::IPC_ID_IDX_MASK;
        self.id2sem.get(&idx).ok_or(SystemError::EINVAL)
    }

    fn current_max_index(&self) -> usize {
        self.id2sem.keys().copied().max().unwrap_or(0)
    }

    /// # semget: create or look up a semaphore set
    pub fn semget(
        &mut self,
        key: SemKey,
        nsems: usize,
        semflg: SemFlags,
    ) -> Result<usize, SystemError> {
        if nsems > SEMMSL {
            return Err(SystemError::EINVAL);
        }

        if key == IPC_PRIVATE {
            return self.create(key, nsems, semflg);
        }

        if let Some(&id) = self.key2id.get(&key) {
            if semflg.contains(SemFlags::IPC_CREAT | SemFlags::IPC_EXCL) {
                return Err(SystemError::EEXIST);
            }
            let set = self.get_by_semid_checked(id)?;
            if nsems > set.sems.len() {
                return Err(SystemError::EINVAL);
            }
            let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
            ipc_perm::ipc_permission(
                &set.kern_ipc_perm,
                semflg.bits() & SemFlags::PERM_MASK.bits(),
                &target_user_ns,
            )?;
            return Ok(id.data());
        }

        if !semflg.contains(SemFlags::IPC_CREAT) {
            return Err(SystemError::ENOENT);
        }
        self.create(key, nsems, semflg)
    }

    fn create(
        &mut self,
        key: SemKey,
        nsems: usize,
        semflg: SemFlags,
    ) -> Result<usize, SystemError> {
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

        self.id2sem
            .try_reserve(1)
            .map_err(|_| SystemError::ENOMEM)?;
        if key != IPC_PRIVATE {
            self.key2id
                .try_reserve(1)
                .map_err(|_| SystemError::ENOMEM)?;
        }
        let sems = KernelSemSet::try_allocate_sems(nsems)?;

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
        let set = KernelSemSet::new(kern_ipc_perm, sems);

        if key != IPC_PRIVATE {
            self.key2id.insert(key, sem_id);
        }
        self.id2sem.insert(ipc_id.idx, set);
        self.total_sems = total_after;

        Ok(sem_id.data())
    }

    /// Simulate sops in order without changing the real semaphore values.
    fn simulate_semop(
        set: &KernelSemSet,
        sops: &[PosixSemBuf],
    ) -> Result<SemopOutcome, SystemError> {
        let mut values = HashMap::with_capacity(sops.len());

        for op in sops {
            let idx = op.sem_num as usize;
            if idx >= set.sems.len() {
                return Err(SystemError::EFBIG);
            }

            let value = values.entry(idx).or_insert(set.sems[idx].val);
            let current = *value;
            if op.sem_op == 0 {
                if current != 0 {
                    return Ok(SemopOutcome::Blocked(SemBlockedOp {
                        semnum: idx,
                        wait_type: SemWaitType::Zero,
                        nowait: (op.sem_flg as u32) & SemFlags::IPC_NOWAIT.bits() != 0,
                    }));
                }
                continue;
            }

            let result = current as i64 + op.sem_op as i64;
            if result > SEMVMX as i64 {
                return Err(SystemError::ERANGE);
            }
            if result < 0 {
                return Ok(SemopOutcome::Blocked(SemBlockedOp {
                    semnum: idx,
                    wait_type: SemWaitType::Increase,
                    nowait: (op.sem_flg as u32) & SemFlags::IPC_NOWAIT.bits() != 0,
                }));
            }
            *value = result as i32;
        }

        Ok(SemopOutcome::Ready(SemopSimulation { values }))
    }

    /// Commit a successful simulation while the manager lock is held.
    fn commit_semop(
        set: &mut KernelSemSet,
        simulation: SemopSimulation,
        pid: Option<Arc<Pid>>,
    ) -> bool {
        let mut values_changed = false;
        for (idx, value) in simulation.values {
            let sem = &mut set.sems[idx];
            values_changed |= sem.val != value;
            sem.val = value;
            sem.pid = pid.clone();
        }
        set.sem_otime = PosixTimeSpec::now().tv_sec;
        values_changed
    }

    /// Complete every executable queue entry without head-of-line blocking.
    fn update_queue(set: &mut KernelSemSet) {
        loop {
            let mut values_changed = false;
            let mut index = 0;

            while index < set.waiters.len() {
                let entry = set.waiters[index].clone();
                match Self::simulate_semop(set, &entry.sops) {
                    Ok(SemopOutcome::Ready(simulation)) => {
                        let changed = Self::commit_semop(set, simulation, entry.pid.clone());
                        values_changed |= changed;
                        set.waiters.remove(index);
                        entry.complete(Ok(0));
                        entry.waker.wake();
                        if changed {
                            break;
                        }
                    }
                    Ok(SemopOutcome::Blocked(blocker)) if blocker.nowait => {
                        set.waiters.remove(index);
                        entry.complete(Err(SystemError::EAGAIN_OR_EWOULDBLOCK));
                        entry.waker.wake();
                    }
                    Ok(SemopOutcome::Blocked(blocker)) => {
                        entry.update_blocker(blocker);
                        index += 1;
                    }
                    Err(error) => {
                        set.waiters.remove(index);
                        entry.complete(Err(error));
                        entry.waker.wake();
                    }
                }
            }

            if !values_changed {
                return;
            }
        }
    }

    fn cancel_queued_entry(
        &mut self,
        semid: SemId,
        entry: &Arc<SemQueueEntry>,
        error: SystemError,
    ) -> Result<usize, SystemError> {
        if let Some(result) = entry.completed_result() {
            return result;
        }

        let set = match self.get_by_semid_checked_mut(semid) {
            Ok(set) => set,
            Err(_) => {
                entry.complete(Err(SystemError::EIDRM));
                return Err(SystemError::EIDRM);
            }
        };
        set.remove_waiter(entry);
        entry.complete(Err(error.clone()));
        Err(error)
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
        if sops
            .iter()
            .any(|op| (op.sem_flg as u32) & SemFlags::SEM_UNDO.bits() != 0)
        {
            return Err(SystemError::ENOSYS);
        }
        let non_blocking = timeout == Some(Duration::ZERO);
        // Check read permission only for all-zero waits; otherwise check write permission
        // to match Linux semantics.
        let alter = sops.iter().any(|op| op.sem_op != 0);

        let target_user_ns = ipcns.user_ns.clone();
        let deadline_ticks = timeout.map(|d| next_n_us_timer_jiffies(d.total_micros()));

        let (waiter, waker) = Waiter::new_pair();
        let timer =
            deadline_ticks.map(|deadline| Timer::new(TimeoutWaker::new(waker.clone()), deadline));
        let pid = ProcessManager::current_pcb().task_pid_ptr(PidType::TGID);

        let entry = {
            let mut guard = ipcns.sem.lock();
            let set = guard.get_by_semid_checked_mut(semid)?;
            // Match Linux: check semnum bounds (EFBIG) before permissions (EACCES).
            if sops.iter().any(|op| op.sem_num as usize >= set.sems.len()) {
                return Err(SystemError::EFBIG);
            }
            ipc_perm::ipc_permission(
                &set.kern_ipc_perm,
                if alter {
                    Self::IPC_WRITE
                } else {
                    Self::IPC_READ
                },
                &target_user_ns,
            )?;

            match Self::simulate_semop(set, sops)? {
                SemopOutcome::Ready(simulation) => {
                    Self::commit_semop(set, simulation, pid);
                    Self::update_queue(set);
                    return Ok(0);
                }
                SemopOutcome::Blocked(blocker) => {
                    if blocker.nowait || non_blocking {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }
                    if deadline_ticks.is_some_and(|deadline| clock() >= deadline) {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }
                    let entry = Arc::new(SemQueueEntry::new(sops, pid, waker.clone(), blocker));
                    set.enqueue_waiter(entry.clone());
                    entry
                }
            }
        };

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

    /// # IPC_RMID: remove the semaphore set and wake all waiters with EIDRM
    pub fn ipc_rmid(&mut self, id: SemId) -> Result<(), SystemError> {
        let decoded = IpcIdAllocator::decode(id.data())?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        let key = {
            let set = self.get_by_semid_checked(id)?;
            ipc_perm::check_control_permission(&set.kern_ipc_perm, &target_user_ns)?;
            set.kern_ipc_perm.key
        };
        let mut set = self
            .id2sem
            .remove(&decoded.idx)
            .ok_or(SystemError::EINVAL)?;
        self.key2id.remove(&SemKey::new(key));
        self.id_allocator.free_idx(decoded.idx);
        self.total_sems = self.total_sems.saturating_sub(set.sems.len());
        set.complete_all_removed();
        Ok(())
    }

    /// # IPC_SET: update permissions (uid/gid/mode) and refresh `sem_ctime`
    pub fn ipc_set(&mut self, id: SemId, semid_ds: PosixSemIdDs) -> Result<(), SystemError> {
        let set = self.get_by_semid_checked_mut(id)?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        ipc_perm::check_control_permission(&set.kern_ipc_perm, &target_user_ns)?;
        let current_user_ns = ProcessManager::current_user_ns();
        set.kern_ipc_perm.copy_from_posix(
            semid_ds.sem_perm.uid(),
            semid_ds.sem_perm.gid(),
            semid_ds.sem_perm.mode(),
            &current_user_ns,
        )?;
        set.sem_ctime = PosixTimeSpec::now().tv_sec;
        Ok(())
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
            ipc_perm::ipc_permission(&set.kern_ipc_perm, Self::IPC_READ, &target_user_ns)?;
        }
        let current_user_ns = ProcessManager::current_user_ns();
        let sem_perm = set.kern_ipc_perm.to_posix(&current_user_ns)?;
        let semid_ds = PosixSemIdDs {
            sem_perm,
            sem_otime: set.sem_otime,
            _sem_otime_high: 0,
            sem_ctime: set.sem_ctime,
            _sem_ctime_high: 0,
            sem_nsems: set.sems.len(),
            _unused1: 0,
            _unused2: 0,
        };
        let ret = if cmd == SemCtlCmd::IpcStat {
            0
        } else {
            set.kern_ipc_perm.id
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
        ipc_perm::ipc_permission(&set.kern_ipc_perm, Self::IPC_READ, &target_user_ns)?;
        if semnum >= set.sems.len() {
            return Err(SystemError::EINVAL);
        }
        match cmd {
            SemCtlCmd::GetVal => Ok(set.sems[semnum].val as usize),
            SemCtlCmd::GetPid => Ok(set.sems[semnum]
                .pid
                .as_ref()
                .map(|pid| pid.pid_vnr().data())
                .unwrap_or(0)),
            SemCtlCmd::GetNcnt => Ok(set.ncnt(semnum)),
            SemCtlCmd::GetZcnt => Ok(set.zcnt(semnum)),
            _ => Err(SystemError::EINVAL),
        }
    }

    /// # SETVAL: set a single semaphore value
    pub fn setval(&mut self, id: SemId, semnum: usize, val: i32) -> Result<(), SystemError> {
        // Match Linux: validate the value (ERANGE), then semnum (EINVAL), then permissions
        // (EACCES).
        if !(0..=SEMVMX).contains(&val) {
            return Err(SystemError::ERANGE);
        }
        let set = self.get_by_semid_checked_mut(id)?;
        if semnum >= set.sems.len() {
            return Err(SystemError::EINVAL);
        }
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        ipc_perm::ipc_permission(&set.kern_ipc_perm, Self::IPC_WRITE, &target_user_ns)?;

        let sem = &mut set.sems[semnum];
        sem.val = val;
        sem.pid = ProcessManager::current_pcb().task_pid_ptr(PidType::TGID);
        set.sem_ctime = PosixTimeSpec::now().tv_sec;
        Self::update_queue(set);
        Ok(())
    }

    /// # SETALL: set values of all semaphores in the set without changes on validation failure
    pub fn setall(&mut self, token: SemSetAllToken, vals: &[u16]) -> Result<(), SystemError> {
        let set = self
            .get_by_semid_checked_mut(token.id)
            .map_err(|_| SystemError::EIDRM)?;
        if vals.len() != token.nsems || vals.len() != set.sems.len() {
            return Err(SystemError::EINVAL);
        }
        if vals.iter().any(|&v| v as i32 > SEMVMX) {
            return Err(SystemError::ERANGE);
        }

        let pid = ProcessManager::current_pcb().task_pid_ptr(PidType::TGID);
        for (i, &v) in vals.iter().enumerate() {
            let sem = &mut set.sems[i];
            sem.val = v as i32;
            sem.pid = pid.clone();
        }
        set.sem_ctime = PosixTimeSpec::now().tv_sec;
        Self::update_queue(set);
        Ok(())
    }

    /// # GETALL: get values of all semaphores in the set
    pub fn getall(&self, id: SemId) -> Result<Vec<u16>, SystemError> {
        let set = self.get_by_semid_checked(id)?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        ipc_perm::ipc_permission(&set.kern_ipc_perm, Self::IPC_READ, &target_user_ns)?;
        Ok(set.sems.iter().map(|s| s.val as u16).collect())
    }

    /// Validate SETALL before the caller accesses the userspace array.
    pub fn prepare_setall(&self, id: SemId) -> Result<SemSetAllToken, SystemError> {
        let set = self.get_by_semid_checked(id)?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        ipc_perm::ipc_permission(&set.kern_ipc_perm, Self::IPC_WRITE, &target_user_ns)?;
        Ok(SemSetAllToken::new(id, set.sems.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::cred::{Kgid, Kuid};

    fn test_perm(id: SemId, key: SemKey, seq: usize) -> IpcPerm {
        IpcPerm {
            id: id.data(),
            key: key.data(),
            uid: Kuid::new(0),
            gid: Kgid::new(0),
            cuid: Kuid::new(0),
            cgid: Kgid::new(0),
            mode: 0o600,
            seq,
        }
    }

    fn insert_test_set(manager: &mut SemManager, key: SemKey, vals: &[i32]) -> SemId {
        let ipc_id = manager.id_allocator.alloc().unwrap();
        let id = SemId::new(ipc_id.raw);
        let sems = KernelSemSet::try_allocate_sems(vals.len()).unwrap();
        let mut set = KernelSemSet::new(test_perm(id, key, ipc_id.seq), sems);
        for (sem, val) in set.sems.iter_mut().zip(vals.iter().copied()) {
            sem.val = val;
        }
        manager.key2id.insert(key, id);
        manager.id2sem.insert(ipc_id.idx, set);
        manager.total_sems += vals.len();
        id
    }

    fn remove_test_set(manager: &mut SemManager, id: SemId) {
        let decoded = IpcIdAllocator::decode(id.data()).unwrap();
        let set = manager.id2sem.remove(&decoded.idx).unwrap();
        manager.key2id.remove(&SemKey::new(set.kern_ipc_perm.key));
        manager.id_allocator.free_idx(decoded.idx);
        manager.total_sems = manager.total_sems.saturating_sub(set.sems.len());
    }

    fn sem_values(manager: &SemManager, id: SemId) -> Vec<i32> {
        manager
            .get_by_semid_checked(id)
            .unwrap()
            .sems
            .iter()
            .map(|sem| sem.val)
            .collect()
    }

    #[test]
    fn prepared_setall_token_returns_eidrm_after_rmid() {
        let mut manager = SemManager::new();
        let id = insert_test_set(&mut manager, SemKey::new(11), &[1, 2]);
        let token = SemSetAllToken::new(id, 2);

        remove_test_set(&mut manager, id);

        assert_eq!(manager.setall(token, &[7, 8]), Err(SystemError::EIDRM));
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

        assert_eq!(manager.setall(token, &[7, 8]), Err(SystemError::EIDRM));
        assert_eq!(sem_values(&manager, new_id), vec![3, 4]);
    }
}
