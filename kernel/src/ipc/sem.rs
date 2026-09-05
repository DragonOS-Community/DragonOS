// SPDX-License-Identifier: GPL-2.0-or-later
//
// System V semaphore subsystem, tracking Linux 6.6 `ipc/sem.c` observable
// behavior, including SEM_UNDO lifecycle accounting.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{cell::Cell, fmt};

use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    ipc::{
        id::IpcIdAllocator,
        ipc_perm::{self, IpcPerm, IpcPermView, PosixIpcPerm},
        sem_undo::{PreparedSemUndoRecordAction, SemUndoGroup, SemUndoRecord},
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
    Queued {
        blocker: SemBlockedOp,
        links: Option<SemWaitLinks>,
    },
    Completed {
        result: Result<usize, SystemError>,
        next_wake: Option<Arc<SemQueueEntry>>,
    },
}

#[derive(Debug)]
struct SemWaitLinks {
    prev: Option<Weak<SemQueueEntry>>,
    next: Option<Arc<SemQueueEntry>>,
}

/// Set-private FIFO. All structural changes require the namespace manager lock.
/// Strong forward / weak backward links avoid cycles; each operation holds at
/// most one entry status lock at a time.
#[derive(Debug, Default)]
struct SemWaitQueue {
    head: Option<Arc<SemQueueEntry>>,
    tail: Option<Arc<SemQueueEntry>>,
}

impl SemWaitQueue {
    fn push_back(&mut self, entry: Arc<SemQueueEntry>) {
        {
            let mut status = entry.status.lock();
            let SemQueueStatus::Queued { links, .. } = &mut *status else {
                panic!("cannot enqueue completed semaphore operation");
            };
            assert!(links.is_none(), "semaphore operation already linked");
            *links = Some(SemWaitLinks {
                prev: self.tail.as_ref().map(Arc::downgrade),
                next: None,
            });
        }
        if let Some(tail) = self.tail.take() {
            let mut status = tail.status.lock();
            let SemQueueStatus::Queued {
                links: Some(links), ..
            } = &mut *status
            else {
                panic!("semaphore queue tail is not linked");
            };
            links.next = Some(entry.clone());
        } else {
            self.head = Some(entry.clone());
        }
        self.tail = Some(entry);
    }

    fn remove(&mut self, entry: &Arc<SemQueueEntry>) -> bool {
        let links = {
            let mut status = entry.status.lock();
            match &mut *status {
                SemQueueStatus::Queued { links, .. } => links.take(),
                SemQueueStatus::Completed { .. } => None,
            }
        };
        let Some(SemWaitLinks { prev, next }) = links else {
            return false;
        };
        let prev = prev.map(|weak| weak.upgrade().expect("linked predecessor is live"));
        if let Some(prev) = prev.as_ref() {
            let mut status = prev.status.lock();
            let SemQueueStatus::Queued {
                links: Some(links), ..
            } = &mut *status
            else {
                panic!("semaphore predecessor is not linked");
            };
            links.next = next.clone();
        } else {
            debug_assert!(self
                .head
                .as_ref()
                .is_some_and(|head| Arc::ptr_eq(head, entry)));
            self.head = next.clone();
        }
        if let Some(next) = next {
            let mut status = next.status.lock();
            let SemQueueStatus::Queued {
                links: Some(links), ..
            } = &mut *status
            else {
                panic!("semaphore successor is not linked");
            };
            links.prev = prev.as_ref().map(Arc::downgrade);
        } else {
            self.tail = prev;
        }
        true
    }

    fn iter(&self) -> SemWaitIter {
        SemWaitIter(self.head.clone())
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.head.is_none()
    }
}

impl Drop for SemWaitQueue {
    fn drop(&mut self) {
        // Detach iteratively, including namespace teardown, never recursively
        // destroy an arbitrarily long chain of Arc-owned entries.
        while let Some(entry) = self.head.clone() {
            self.remove(&entry);
        }
    }
}

struct SemWaitIter(Option<Arc<SemQueueEntry>>);

impl Iterator for SemWaitIter {
    type Item = Arc<SemQueueEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.0.take()?;
        self.0 = match &*entry.status.lock() {
            SemQueueStatus::Queued {
                links: Some(links), ..
            } => links.next.clone(),
            _ => None,
        };
        Some(entry)
    }
}

/// An Arc-owned queue entry shared by the set and the blocked caller.
#[derive(Debug)]
struct SemQueueEntry {
    sops: Vec<PosixSemBuf>,
    pid: Option<Arc<Pid>>,
    undo_group: Option<Arc<SemUndoGroup>>,
    undo_record: SpinLock<Option<SemUndoRecord>>,
    waker: Arc<Waker>,
    scratch: SpinLock<SemopScratch>,
    status: SpinLock<SemQueueStatus>,
}

impl SemQueueEntry {
    fn new_prepared(
        sops: Vec<PosixSemBuf>,
        pid: Option<Arc<Pid>>,
        undo_group: Option<Arc<SemUndoGroup>>,
        undo_record: Option<SemUndoRecord>,
        waker: Arc<Waker>,
        scratch: SemopScratch,
        blocker: SemBlockedOp,
    ) -> Self {
        debug_assert_eq!(
            undo_group.is_some(),
            undo_record.is_some(),
            "queued SEM_UNDO group and prepared record must be captured together"
        );
        debug_assert!(
            undo_group.is_some()
                || sops
                    .iter()
                    .all(|op| (op.sem_flg as u32) & SemFlags::SEM_UNDO.bits() == 0),
            "queued SEM_UNDO entry requires a captured undo group"
        );
        Self {
            scratch: SpinLock::new(scratch),
            sops,
            pid,
            undo_group,
            undo_record: SpinLock::new(undo_record),
            waker,
            status: SpinLock::new(SemQueueStatus::Queued {
                blocker,
                links: None,
            }),
        }
    }

    fn prepare_sops(sops: &[PosixSemBuf]) -> Result<Vec<PosixSemBuf>, SystemError> {
        let mut owned_sops = Vec::new();
        owned_sops
            .try_reserve_exact(sops.len())
            .map_err(|_| SystemError::ENOMEM)?;
        owned_sops.extend_from_slice(sops);
        Ok(owned_sops)
    }

    #[cfg(test)]
    fn new(
        sops: &[PosixSemBuf],
        pid: Option<Arc<Pid>>,
        waker: Arc<Waker>,
        blocker: SemBlockedOp,
    ) -> Self {
        Self::new_prepared(
            Self::prepare_sops(sops).unwrap(),
            pid,
            None,
            None,
            waker,
            SemopScratch::try_new(sops.len()).unwrap(),
            blocker,
        )
    }

    fn completed_result(&self) -> Option<Result<usize, SystemError>> {
        match &*self.status.lock() {
            SemQueueStatus::Queued { .. } => None,
            SemQueueStatus::Completed { result, .. } => Some(result.clone()),
        }
    }

    fn update_blocker(&self, blocker: SemBlockedOp) {
        let mut status = self.status.lock();
        if let SemQueueStatus::Queued {
            blocker: current, ..
        } = &mut *status
        {
            *current = blocker;
        }
    }

    fn complete(&self, result: Result<usize, SystemError>) -> bool {
        let mut status = self.status.lock();
        if matches!(&*status, SemQueueStatus::Completed { .. }) {
            return false;
        }
        assert!(
            matches!(&*status, SemQueueStatus::Queued { links: None, .. }),
            "semaphore operation must be unlinked before completion"
        );
        *status = SemQueueStatus::Completed {
            result,
            next_wake: None,
        };
        true
    }

    fn is_waiting_on(&self, semnum: usize, wait_type: SemWaitType) -> bool {
        matches!(
            &*self.status.lock(),
            SemQueueStatus::Queued { blocker, .. }
                if blocker.semnum == semnum && blocker.wait_type == wait_type
        )
    }
}

/// Per-operation completion list. The existing entry status lock protects the
/// link; no allocation or scheduler operation is needed while the set is locked.
/// Declare this before manager guards so all exits wake only after unlocking.
#[derive(Default)]
pub(crate) struct SemWakeBatch {
    head: Option<Arc<SemQueueEntry>>,
    tail: Option<Arc<SemQueueEntry>>,
}

impl SemWakeBatch {
    fn complete(&mut self, entry: Arc<SemQueueEntry>, result: Result<usize, SystemError>) {
        if !entry.complete(result) {
            return;
        }
        if let Some(tail) = self.tail.take() {
            if let SemQueueStatus::Completed { next_wake, .. } = &mut *tail.status.lock() {
                *next_wake = Some(entry.clone());
            }
        } else {
            self.head = Some(entry.clone());
        }
        self.tail = Some(entry);
    }

    pub(crate) fn wake_all(&mut self) {
        self.tail = None;
        while let Some(entry) = self.head.take() {
            self.head = match &mut *entry.status.lock() {
                SemQueueStatus::Completed { next_wake, .. } => next_wake.take(),
                SemQueueStatus::Queued { .. } => unreachable!("wake batch contains queued entry"),
            };
            // Release the status lock before entering the scheduler. Detaching
            // each link also prevents recursive Arc destruction for large batches.
            entry.waker.wake();
        }
    }
}

impl Drop for SemWakeBatch {
    fn drop(&mut self) {
        self.wake_all();
    }
}

/// Selects one of the set-global pending queues.
#[derive(Debug, Clone, Copy)]
enum SemPendingQueue {
    Const,
    Alter,
}

/// Semaphore set
#[derive(Debug)]
pub struct KernelSemSet {
    /// Groups that can carry undo debt for this set, including queued operations.
    undo_groups: Vec<Weak<SemUndoGroup>>,
    /// Permission information
    pub kern_ipc_perm: IpcPerm,
    /// Semaphores in the set
    pub sems: Vec<KernelSem>,
    /// Time of the last `semop`
    pub sem_otime: i64,
    /// Time of the last metadata change
    pub sem_ctime: i64,
    /// Pending operation groups containing only zero-wait operations
    pending_const: SemWaitQueue,
    /// Pending operation groups containing at least one altering operation
    pending_alter: SemWaitQueue,
}

impl KernelSemSet {
    /// Live associations survive SETVAL/SETALL. Only RMID or dead groups
    /// remove them, so an existing undo record proves prior association.
    /// Insufficient spare capacity requests an unlocked preparation/retry.
    /// On success, spare retains any replaced allocation for unlocked disposal.
    fn ensure_undo_group_registered_prepared(
        &mut self,
        group: &Arc<SemUndoGroup>,
        spare: &mut Vec<Weak<SemUndoGroup>>,
    ) -> Result<(), usize> {
        debug_assert!(spare.is_empty());
        let candidate = Arc::downgrade(group);
        if self.undo_groups.iter().any(|weak| weak.ptr_eq(&candidate)) {
            return Ok(());
        }
        if self.undo_groups.len() == self.undo_groups.capacity() {
            self.compact_undo_registry();
        }
        if self.undo_groups.len() == self.undo_groups.capacity() {
            if spare.capacity() <= self.undo_groups.len() {
                return Err(self.undo_groups.len().saturating_mul(2).max(4));
            }
            spare.append(&mut self.undo_groups);
            core::mem::swap(&mut self.undo_groups, spare);
        }
        self.undo_groups.push(candidate);
        Ok(())
    }

    fn compact_undo_registry(&mut self) {
        self.undo_groups.retain(|weak| weak.strong_count() != 0);
    }

    /// Request/publish smaller storage without allocating under the manager
    /// lock. Empty registries are moved into retired without installing spare.
    /// The caller must drop both buffers after unlocking.
    fn shrink_undo_registry_prepared(
        &mut self,
        spare: &mut Vec<Weak<SemUndoGroup>>,
        retired: &mut Vec<Weak<SemUndoGroup>>,
    ) -> usize {
        debug_assert!(spare.is_empty());
        debug_assert!(retired.is_empty() && retired.capacity() == 0);
        let len = self.undo_groups.len();
        if len == 0 {
            core::mem::swap(&mut self.undo_groups, retired);
            return 0;
        }
        if self.undo_groups.capacity() <= 4 || len > self.undo_groups.capacity() / 4 {
            return 0;
        }
        if spare.capacity() < len {
            return len.saturating_mul(2).max(4);
        }
        if spare.capacity() < self.undo_groups.capacity() {
            spare.append(&mut self.undo_groups);
            core::mem::swap(&mut self.undo_groups, spare);
        }
        0
    }

    fn try_allocate_sems(nsems: usize) -> Result<Vec<KernelSem>, SystemError> {
        let mut sems = Vec::new();
        sems.try_reserve_exact(nsems)
            .map_err(|_| SystemError::ENOMEM)?;
        sems.resize(nsems, KernelSem { val: 0, pid: None });
        Ok(sems)
    }

    fn new(kern_ipc_perm: IpcPerm, sems: Vec<KernelSem>) -> Self {
        KernelSemSet {
            undo_groups: Vec::new(),
            kern_ipc_perm,
            sems,
            sem_otime: 0,
            sem_ctime: PosixTimeSpec::now().tv_sec,
            pending_const: SemWaitQueue::default(),
            pending_alter: SemWaitQueue::default(),
        }
    }

    fn pending_queue_for(sops: &[PosixSemBuf]) -> SemPendingQueue {
        if sops.iter().any(|op| op.sem_op != 0) {
            SemPendingQueue::Alter
        } else {
            SemPendingQueue::Const
        }
    }

    /// The prepared entry itself owns the queue links; publication cannot allocate.
    fn enqueue_waiter(&mut self, waiter: Arc<SemQueueEntry>) {
        let queue = match Self::pending_queue_for(&waiter.sops) {
            SemPendingQueue::Const => &mut self.pending_const,
            SemPendingQueue::Alter => &mut self.pending_alter,
        };
        queue.push_back(waiter);
    }

    fn remove_waiter(&mut self, target: &Arc<SemQueueEntry>) {
        self.remove_pending(Self::pending_queue_for(&target.sops), target);
    }

    #[cfg(test)]
    fn pending_is_empty(&self) -> bool {
        self.pending_const.is_empty() && self.pending_alter.is_empty()
    }

    fn pending_iter(&self, queue: SemPendingQueue) -> SemWaitIter {
        match queue {
            SemPendingQueue::Const => self.pending_const.iter(),
            SemPendingQueue::Alter => self.pending_alter.iter(),
        }
    }

    fn remove_pending(&mut self, queue: SemPendingQueue, entry: &Arc<SemQueueEntry>) {
        match queue {
            SemPendingQueue::Const => self.pending_const.remove(entry),
            SemPendingQueue::Alter => self.pending_alter.remove(entry),
        };
    }

    /// Publish removal under the manager lock; the caller wakes after unlocking.
    fn complete_all_removed(&mut self, wakes: &mut SemWakeBatch) {
        for entry in self.pending_const.iter() {
            self.pending_const.remove(&entry);
            wakes.complete(entry, Err(SystemError::EIDRM));
        }
        for entry in self.pending_alter.iter() {
            self.pending_alter.remove(&entry);
            wakes.complete(entry, Err(SystemError::EIDRM));
        }
    }

    fn ncnt(&self, semnum: usize) -> usize {
        self.pending_const
            .iter()
            .chain(self.pending_alter.iter())
            .filter(|entry| entry.is_waiting_on(semnum, SemWaitType::Increase))
            .count()
    }

    fn zcnt(&self, semnum: usize) -> usize {
        self.pending_const
            .iter()
            .chain(self.pending_alter.iter())
            .filter(|entry| entry.is_waiting_on(semnum, SemWaitType::Zero))
            .count()
    }
}

#[derive(Debug)]
struct SemopScratchEntry {
    semnum: usize,
    initial_val: i32,
    virtual_val: i32,
    initial_adj: i16,
    virtual_adj: i16,
}

#[derive(Debug)]
struct SemopScratch {
    entries: Vec<SemopScratchEntry>,
}

impl SemopScratch {
    fn try_new(capacity: usize) -> Result<Self, SystemError> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| SystemError::ENOMEM)?;
        Ok(Self { entries })
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    fn entry_for(
        &mut self,
        set: &KernelSemSet,
        semnum: usize,
        undo: Option<&SemUndoRecord>,
    ) -> Result<&mut SemopScratchEntry, SystemError> {
        if let Some(index) = self.entries.iter().position(|entry| entry.semnum == semnum) {
            return Ok(&mut self.entries[index]);
        }

        if self.entries.len() == self.entries.capacity() {
            return Err(SystemError::ENOMEM);
        }

        let initial_val = set.sems[semnum].val;
        let initial_adj = undo
            .map(|record| record.adjustment(semnum))
            .unwrap_or_default();
        self.entries.push(SemopScratchEntry {
            semnum,
            initial_val,
            virtual_val: initial_val,
            initial_adj,
            virtual_adj: initial_adj,
        });
        Ok(self
            .entries
            .last_mut()
            .expect("SEM_UNDO scratch entry was just inserted"))
    }
}

/// Fixed-capacity virtual semaphore state produced by `SemopScratch`.
#[derive(Debug)]
struct SemopSimulation {
    entry_count: usize,
}

impl SemopSimulation {
    #[cfg(test)]
    fn empty_for_test() -> Self {
        Self { entry_count: 0 }
    }
}

/// Result of an attempted `semop` execution
#[derive(Debug)]
enum SemopOutcome {
    Ready(SemopSimulation),
    Blocked(SemBlockedOp),
}

impl SemopOutcome {
    #[cfg(test)]
    fn ready_for_test(self) -> SemopSimulation {
        match self {
            Self::Ready(simulation) => simulation,
            Self::Blocked(_) => panic!("expected ready semop outcome"),
        }
    }
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

    #[allow(dead_code)]
    fn validate_semid_nsems(&self, semid: SemId) -> Result<usize, SystemError> {
        Ok(self.get_by_semid_checked(semid)?.sems.len())
    }

    /// Test-only setup; production reserves registry storage before locking.
    #[cfg(test)]
    fn ensure_undo_group_registered(
        &mut self,
        group: &Arc<SemUndoGroup>,
        semid: SemId,
    ) -> Result<(), SystemError> {
        let set = self.get_by_semid_checked_mut(semid)?;
        let mut spare = Vec::new();
        if let Err(capacity) = set.ensure_undo_group_registered_prepared(group, &mut spare) {
            spare
                .try_reserve(capacity)
                .map_err(|_| SystemError::ENOMEM)?;
            set.ensure_undo_group_registered_prepared(group, &mut spare)
                .unwrap();
        }
        Ok(())
    }

    fn with_undo_record_mut<R>(
        group: &Arc<SemUndoGroup>,
        semid: SemId,
        f: impl FnOnce(&mut SemUndoRecord) -> R,
    ) -> Option<R> {
        group.with_record_mut(semid, f)
    }

    pub(crate) fn unregister_undo_group(&mut self, semid: SemId, group: &Arc<SemUndoGroup>) {
        if let Ok(set) = self.get_by_semid_checked_mut(semid) {
            let target = Arc::downgrade(group);
            set.undo_groups.retain(|weak| !weak.ptr_eq(&target));
        }
    }

    /// Reclaim a target registry, never allocating or freeing its buffer under
    /// the namespace lock. Concurrent growth can make a shrink unnecessary or
    /// consume the spare capacity; in either case leave the live registry intact.
    pub(crate) fn shrink_undo_registry(ipcns: &Arc<IpcNamespace>, semid: SemId) {
        let mut spare = Vec::new();
        let mut retired = Vec::new();
        let needed = {
            let mut manager = ipcns.sem.lock();
            let Ok(set) = manager.get_by_semid_checked_mut(semid) else {
                return;
            };
            set.shrink_undo_registry_prepared(&mut spare, &mut retired)
        };
        if needed == 0 || spare.try_reserve_exact(needed).is_err() {
            return;
        }
        // Allocation is best-effort reclamation, not part of syscall success.
        let mut manager = ipcns.sem.lock();
        if let Ok(set) = manager.get_by_semid_checked_mut(semid) {
            set.shrink_undo_registry_prepared(&mut spare, &mut retired);
        }
    }

    fn release_unused_undo_group(
        ipcns: &Arc<IpcNamespace>,
        semid: SemId,
        group: &Arc<SemUndoGroup>,
    ) {
        {
            let mut manager = ipcns.sem.lock();
            let Ok(set) = manager.get_by_semid_checked(semid) else {
                return;
            };
            if group.with_record_mut(semid, |_| ()).is_some()
                || set
                    .pending_const
                    .iter()
                    .chain(set.pending_alter.iter())
                    .any(|entry| {
                        entry
                            .undo_group
                            .as_ref()
                            .is_some_and(|owner| Arc::ptr_eq(owner, group))
                    })
            {
                return;
            }
            // Other Missing preparations outside the lock re-register before
            // publication. Existing records (even zero debt) keep their link.
            manager.unregister_undo_group(semid, group);
        }
        Self::shrink_undo_registry(ipcns, semid);
    }

    pub(crate) fn clear_undo_for_setval(&mut self, semid: SemId, semnum: usize) {
        let Ok(set) = self.get_by_semid_checked_mut(semid) else {
            return;
        };
        let mut saw_stale = false;
        for weak in set.undo_groups.iter() {
            let Some(group) = weak.upgrade() else {
                saw_stale = true;
                continue;
            };
            Self::with_undo_record_mut(&group, semid, |record| {
                if semnum < record.adjustment_count() {
                    record.clear_adjustment(semnum);
                }
            });
        }
        if saw_stale {
            set.compact_undo_registry();
        }
    }

    pub(crate) fn clear_undo_for_setall(&mut self, semid: SemId) {
        let Ok(set) = self.get_by_semid_checked_mut(semid) else {
            return;
        };
        let mut saw_stale = false;
        for weak in set.undo_groups.iter() {
            let Some(group) = weak.upgrade() else {
                saw_stale = true;
                continue;
            };
            Self::with_undo_record_mut(&group, semid, |record| record.clear_all_adjustments());
        }
        if saw_stale {
            set.compact_undo_registry();
        }
    }

    pub(crate) fn discard_undo_for_rmid(&mut self, semid: SemId) {
        let Ok(set) = self.get_by_semid_checked_mut(semid) else {
            return;
        };
        let mut saw_stale = false;
        for weak in set.undo_groups.iter() {
            let Some(group) = weak.upgrade() else {
                saw_stale = true;
                continue;
            };
            group.remove_record(semid);
        }
        if saw_stale {
            set.compact_undo_registry();
        }
    }

    #[allow(dead_code)]
    pub(crate) fn prune_and_apply_setval_undo(&mut self, semid: SemId, semnum: usize) {
        self.clear_undo_for_setval(semid, semnum);
    }

    #[allow(dead_code)]
    pub(crate) fn prune_and_apply_setall_undo(&mut self, semid: SemId) {
        self.clear_undo_for_setall(semid);
    }

    #[allow(dead_code)]
    pub(crate) fn remove_undo_records_for_rmid(&mut self, semid: SemId) {
        self.discard_undo_for_rmid(semid);
    }

    #[cfg(test)]
    fn update_queue_for_test(&mut self, semid: SemId) {
        let Ok(set) = self.get_by_semid_checked_mut(semid) else {
            return;
        };
        Self::update_queue(set, semid, &mut SemWakeBatch::default());
    }

    #[cfg(test)]
    fn live_undo_group_count_for_test(&self) -> usize {
        let mut groups: Vec<Weak<SemUndoGroup>> = Vec::new();
        for weak in self.id2sem.values().flat_map(|set| &set.undo_groups) {
            if weak.strong_count() != 0 && !groups.iter().any(|old| old.ptr_eq(weak)) {
                groups.push(weak.clone());
            }
        }
        groups.len()
    }

    #[cfg(test)]
    fn undo_registry_contains_for_test(&self, group: &Arc<SemUndoGroup>) -> bool {
        self.id2sem
            .values()
            .flat_map(|set| &set.undo_groups)
            .any(|weak| weak.ptr_eq(&Arc::downgrade(group)))
    }

    #[cfg(test)]
    fn namespace_lifecycle_invariant_for_test(&self) -> bool {
        self.id2sem
            .values()
            .flat_map(|set| &set.undo_groups)
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
            if nsems > set.sems.len() {
                return Err(SystemError::EINVAL);
            }
            let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
            ipc_perm::ipc_permission(
                &set.kern_ipc_perm,
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

    /// Simulate sops in order without changing shared semaphore values or undo records.
    fn simulate_semop(
        set: &KernelSemSet,
        sops: &[PosixSemBuf],
        undo: Option<&mut SemUndoRecord>,
        scratch: &mut SemopScratch,
    ) -> Result<SemopOutcome, SystemError> {
        scratch.clear();

        for op in sops {
            let idx = op.sem_num as usize;
            if idx >= set.sems.len() {
                return Err(SystemError::EFBIG);
            }

            let has_undo = (op.sem_flg as u32) & SemFlags::SEM_UNDO.bits() != 0;
            let entry = scratch.entry_for(set, idx, undo.as_deref())?;
            let current = entry.virtual_val;
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

            if has_undo {
                let next_adj = entry.virtual_adj as i32 - op.sem_op as i32;
                if !(i16::MIN as i32..=i16::MAX as i32).contains(&next_adj) {
                    return Err(SystemError::ERANGE);
                }
                entry.virtual_adj = next_adj as i16;
            }
            entry.virtual_val = result as i32;
        }

        Ok(SemopOutcome::Ready(SemopSimulation {
            entry_count: scratch.entries.len(),
        }))
    }

    /// Commit a successful simulation while the manager lock is held.
    fn commit_semop(
        set: &mut KernelSemSet,
        simulation: SemopSimulation,
        scratch: &SemopScratch,
        pid: Option<Arc<Pid>>,
        mut undo: Option<&mut SemUndoRecord>,
    ) -> bool {
        let mut values_changed = false;
        for entry in scratch.entries.iter().take(simulation.entry_count) {
            let sem = &mut set.sems[entry.semnum];
            values_changed |= entry.initial_val != entry.virtual_val;
            sem.val = entry.virtual_val;
            sem.pid = pid.clone();
            if entry.virtual_adj != entry.initial_adj {
                if let Some(record) = undo.as_deref_mut() {
                    record.set_adjustment(entry.semnum, entry.virtual_adj);
                }
            }
        }
        set.sem_otime = PosixTimeSpec::now().tv_sec;
        values_changed
    }

    /// Scan one pending queue without head-of-line blocking.
    fn scan_pending_queue(
        set: &mut KernelSemSet,
        queue: SemPendingQueue,
        wakes: &mut SemWakeBatch,
    ) -> bool {
        // Iterator captures the successor before the current entry is unlinked.
        for entry in set.pending_iter(queue) {
            if let Some(group) = entry.undo_group.as_ref() {
                let result = {
                    let mut record_slot = entry.undo_record.lock_irqsave();
                    let Some(record) = record_slot.take() else {
                        set.remove_pending(queue, &entry);
                        wakes.complete(entry.clone(), Err(SystemError::EINVAL));
                        continue;
                    };
                    match group.with_prepared_record_noalloc(record, |record| {
                        match Self::retry_queued_undo_entry(set, &entry, record) {
                            Ok(Some(changed)) => {
                                PreparedSemUndoRecordAction::Publish(Ok(Some(changed)))
                            }
                            Ok(None) => PreparedSemUndoRecordAction::Keep(Ok(None)),
                            Err(error) => PreparedSemUndoRecordAction::Keep(Err(error)),
                        }
                    }) {
                        Ok((result, kept_record)) => {
                            *record_slot = kept_record;
                            result
                        }
                        Err(error) => Err(error),
                    }
                };

                match result {
                    Ok(Some(changed)) => {
                        set.remove_pending(queue, &entry);
                        wakes.complete(entry.clone(), Ok(0));
                        if changed {
                            return true;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        set.remove_pending(queue, &entry);
                        wakes.complete(entry.clone(), Err(error));
                    }
                }
                continue;
            }

            let mut scratch = entry.scratch.lock();
            match Self::simulate_semop(set, &entry.sops, None, &mut scratch) {
                Ok(SemopOutcome::Ready(simulation)) => {
                    let changed =
                        Self::commit_semop(set, simulation, &scratch, entry.pid.clone(), None);
                    set.remove_pending(queue, &entry);
                    wakes.complete(entry.clone(), Ok(0));
                    if changed {
                        return true;
                    }
                }
                Ok(SemopOutcome::Blocked(blocker)) if blocker.nowait => {
                    set.remove_pending(queue, &entry);
                    wakes.complete(entry.clone(), Err(SystemError::EAGAIN_OR_EWOULDBLOCK));
                }
                Ok(SemopOutcome::Blocked(blocker)) => {
                    entry.update_blocker(blocker);
                }
                Err(error) => {
                    set.remove_pending(queue, &entry);
                    wakes.complete(entry.clone(), Err(error));
                }
            }
        }
        false
    }

    /// Complete executable const entries before altering entries.
    fn update_queue(set: &mut KernelSemSet, _semid: SemId, wakes: &mut SemWakeBatch) {
        loop {
            let const_changed = Self::scan_pending_queue(set, SemPendingQueue::Const, wakes);
            debug_assert!(!const_changed);
            if !Self::scan_pending_queue(set, SemPendingQueue::Alter, wakes) {
                return;
            }
        }
    }

    fn retry_queued_undo_entry(
        set: &mut KernelSemSet,
        entry: &Arc<SemQueueEntry>,
        record: &mut SemUndoRecord,
    ) -> Result<Option<bool>, SystemError> {
        if record.adjustment_count() != set.sems.len() {
            return Err(SystemError::EINVAL);
        }
        let mut scratch = entry.scratch.lock();
        match Self::simulate_semop(set, &entry.sops, Some(record), &mut scratch) {
            Ok(SemopOutcome::Ready(simulation)) => Ok(Some(Self::commit_semop(
                set,
                simulation,
                &scratch,
                entry.pid.clone(),
                Some(record),
            ))),
            Ok(SemopOutcome::Blocked(blocker)) => {
                if blocker.nowait {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                } else {
                    entry.update_blocker(blocker);
                    Ok(None)
                }
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn replay_sem_undo_adjustments(
        &mut self,
        semid: SemId,
        adjustments: &[i16],
        exiting_tgid: Option<Arc<Pid>>,
        wakes: &mut SemWakeBatch,
    ) {
        let Ok(set) = self.get_by_semid_checked_mut(semid) else {
            return;
        };

        for (sem, adjustment) in set.sems.iter_mut().zip(adjustments.iter().copied()) {
            if adjustment == 0 {
                continue;
            }

            let next = (sem.val as i64 + adjustment as i64).clamp(0, SEMVMX as i64) as i32;
            sem.val = next;
            sem.pid = exiting_tgid.clone();
        }

        set.sem_otime = PosixTimeSpec::now().tv_sec;
        Self::update_queue(set, semid, wakes);
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

        if let Ok(set) = self.get_by_semid_checked_mut(semid) {
            if let Some(result) = entry.completed_result() {
                return result;
            }
            set.remove_waiter(entry);
            if entry.complete(Err(error.clone())) {
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
        let registered_missing = Cell::new(false);
        // Declare before all entries and manager guards. Failed/timeout-only
        // operations have no published record for final-owner exit to enumerate.
        defer::defer!({
            if registered_missing.get() {
                if let Some(group) = undo_group.as_ref() {
                    Self::release_unused_undo_group(ipcns, semid, group);
                }
            }
        });
        let mut immediate_scratch = SemopScratch::try_new(sops.len())?;
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
                    SemopScratch::try_new(sops.len())?,
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
        let mut registry_spare = Vec::new();
        let mut registry_capacity_needed = 0;
        let entry = loop {
            // Revalidate after unlocked undo-registry preparation.
            if registry_capacity_needed != 0 {
                registry_spare
                    .try_reserve(registry_capacity_needed)
                    .map_err(|_| SystemError::ENOMEM)?;
                registry_capacity_needed = 0;
            }
            let nsems = {
                let guard = ipcns.sem.lock();
                let set = guard.get_by_semid_checked(semid)?;
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
                set.sems.len()
            };

            let prepared_undo = if let Some(group) = undo_group.as_ref() {
                let record = group.prepare_record(semid, nsems)?;
                let entry = Arc::try_new(SemQueueEntry::new_prepared(
                    SemQueueEntry::prepare_sops(sops)?,
                    pid.clone(),
                    Some(group.clone()),
                    Some(record),
                    waker.clone(),
                    SemopScratch::try_new(sops.len())?,
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
                // Only a published record can take the fast path. Missing
                // (including queued) records must be linked before publication.
                // Full semid was rechecked above, so Existing cannot refer to a
                // destroyed/reused set. SETVAL/SETALL retain live associations.
                let already_associated = prepared_entry
                    .undo_record
                    .lock_irqsave()
                    .as_ref()
                    .is_some_and(SemUndoRecord::was_existing);
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
                    registered_missing.set(true);
                }
                let mut record_slot = prepared_entry.undo_record.lock_irqsave();
                let prepared_record = record_slot
                    .take()
                    .expect("prepared SEM_UNDO entry owns its record");
                if prepared_record.adjustment_count() != nsems {
                    *record_slot = Some(prepared_record);
                    continue;
                }
                let set = guard.get_by_semid_checked_mut(semid)?;
                let (outcome, kept_record) = undo_group
                    .as_ref()
                    .expect("SEM_UNDO operation has a current group")
                    .with_prepared_record_noalloc(prepared_record, |record| {
                        let outcome =
                            Self::simulate_semop(set, sops, Some(record), &mut immediate_scratch);
                        match outcome {
                            Ok(SemopOutcome::Ready(simulation)) => {
                                Self::commit_semop(
                                    set,
                                    simulation,
                                    &immediate_scratch,
                                    pid.clone(),
                                    Some(record),
                                );
                                PreparedSemUndoRecordAction::Publish(Ok(None))
                            }
                            Ok(SemopOutcome::Blocked(blocker)) => {
                                PreparedSemUndoRecordAction::Keep(Ok(Some(blocker)))
                            }
                            Err(error) => PreparedSemUndoRecordAction::Keep(Err(error)),
                        }
                    })?;
                *record_slot = kept_record;
                match outcome? {
                    None => {
                        drop(record_slot);
                        Self::update_queue(set, semid, &mut wakes);
                        return Ok(0);
                    }
                    Some(blocker) => {
                        if blocker.nowait || non_blocking {
                            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                        }
                        if deadline_ticks.is_some_and(|deadline| clock() >= deadline) {
                            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                        }
                        drop(record_slot);
                        prepared_entry.update_blocker(blocker);
                        set.enqueue_waiter(prepared_entry.clone());
                        break prepared_entry;
                    }
                }
            } else {
                let set = guard.get_by_semid_checked_mut(semid)?;
                match Self::simulate_semop(set, sops, None, &mut immediate_scratch)? {
                    SemopOutcome::Ready(simulation) => {
                        Self::commit_semop(set, simulation, &immediate_scratch, pid, None);
                        Self::update_queue(set, semid, &mut wakes);
                        return Ok(0);
                    }
                    SemopOutcome::Blocked(blocker) => {
                        if blocker.nowait || non_blocking {
                            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                        }
                        if deadline_ticks.is_some_and(|deadline| clock() >= deadline) {
                            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                        }
                        let prepared_entry = plain_prepared_entry
                            .as_ref()
                            .expect("plain queued semop entry is preallocated");
                        prepared_entry.update_blocker(blocker);
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

    /// # IPC_RMID: remove the semaphore set and wake all waiters with EIDRM
    pub(crate) fn ipc_rmid(
        &mut self,
        id: SemId,
        wakes: &mut SemWakeBatch,
    ) -> Result<(), SystemError> {
        let decoded = IpcIdAllocator::decode(id.data())?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        let key = {
            let set = self.get_by_semid_checked(id)?;
            ipc_perm::check_control_permission(&set.kern_ipc_perm, &target_user_ns)?;
            set.kern_ipc_perm.key
        };
        self.discard_undo_for_rmid(id);
        let mut set = self
            .id2sem
            .remove(&decoded.idx)
            .ok_or(SystemError::EINVAL)?;
        self.key2id.remove(&SemKey::new(key));
        self.total_sems = self.total_sems.saturating_sub(set.sems.len());
        set.complete_all_removed(wakes);
        self.id_allocator.free_idx(decoded.idx);
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
            if semnum >= set.sems.len() {
                return Err(SystemError::EINVAL);
            }
            let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
            ipc_perm::ipc_permission(&set.kern_ipc_perm, Self::IPC_WRITE, &target_user_ns)?;
            set.sems.len()
        };
        debug_assert!(semnum < nsems);

        self.clear_undo_for_setval(id, semnum);
        let set = self.get_by_semid_checked_mut(id)?;
        let sem = &mut set.sems[semnum];
        sem.val = val;
        sem.pid = ProcessManager::current_pcb().task_pid_ptr(PidType::TGID);
        set.sem_ctime = PosixTimeSpec::now().tv_sec;
        Self::update_queue(set, id, wakes);
        Ok(())
    }

    /// # SETALL: set values of all semaphores in the set without changes on validation failure
    pub(crate) fn setall(
        &mut self,
        token: SemSetAllToken,
        vals: &[u16],
        wakes: &mut SemWakeBatch,
    ) -> Result<(), SystemError> {
        let set_nsems = self
            .get_by_semid_checked(token.id)
            .map_err(|_| SystemError::EIDRM)?
            .sems
            .len();
        if vals.len() != token.nsems || vals.len() != set_nsems {
            return Err(SystemError::EINVAL);
        }
        if vals.iter().any(|&v| v as i32 > SEMVMX) {
            return Err(SystemError::ERANGE);
        }

        self.clear_undo_for_setall(token.id);
        let set = self
            .get_by_semid_checked_mut(token.id)
            .map_err(|_| SystemError::EIDRM)?;
        let pid = ProcessManager::current_pcb().task_pid_ptr(PidType::TGID);
        for (i, &v) in vals.iter().enumerate() {
            let sem = &mut set.sems[i];
            sem.val = v as i32;
            sem.pid = pid.clone();
        }
        set.sem_ctime = PosixTimeSpec::now().tv_sec;
        Self::update_queue(set, token.id, wakes);
        Ok(())
    }

    /// # GETALL: get values of all semaphores in the set
    pub fn getall(&self, id: SemId) -> Result<Vec<u16>, SystemError> {
        let set = self.get_by_semid_checked(id)?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        ipc_perm::ipc_permission(&set.kern_ipc_perm, Self::IPC_READ, &target_user_ns)?;
        let mut vals = Vec::new();
        vals.try_reserve_exact(set.sems.len())
            .map_err(|_| SystemError::ENOMEM)?;
        vals.extend(set.sems.iter().map(|s| s.val as u16));
        Ok(vals)
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
    use crate::{
        ipc::sem_undo::detach_sem_undo,
        process::{
            cred::{Kgid, Kuid},
            fork::CloneFlags,
            namespace::ipc_namespace::INIT_IPC_NAMESPACE,
            namespace::pid_namespace::INIT_PID_NAMESPACE,
            KernelStack, ProcessControlBlock, RawPid,
        },
    };

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

    fn test_ipc_ns() -> Arc<IpcNamespace> {
        INIT_IPC_NAMESPACE.copy_ipc_ns(
            &CloneFlags::CLONE_NEWIPC,
            INIT_IPC_NAMESPACE.user_ns.clone(),
        )
    }

    fn test_pcb_with_group(
        ipc_ns: &Arc<IpcNamespace>,
    ) -> (Arc<ProcessControlBlock>, Arc<SemUndoGroup>) {
        let pcb = ProcessControlBlock::new_idle(0, KernelStack::new().unwrap());
        let group = pcb.ensure_sem_undo_group(ipc_ns).unwrap();
        (pcb, group)
    }

    fn enqueue_test_waiter(
        set: &mut KernelSemSet,
        sops: &[PosixSemBuf],
        blocker: SemBlockedOp,
    ) -> Arc<SemQueueEntry> {
        let (_waiter, waker) = Waiter::new_pair();
        let entry = Arc::new(SemQueueEntry::new(sops, None, waker, blocker));
        set.enqueue_waiter(entry.clone());
        entry
    }

    #[test]
    fn pending_links_remove_middle_head_tail_and_preserve_fifo() {
        let mut manager = SemManager::new();
        let id = insert_test_set(&mut manager, SemKey::new(153), &[0]);
        let set = manager.get_by_semid_checked_mut(id).unwrap();
        let ops = [PosixSemBuf {
            sem_num: 0,
            sem_op: -1,
            sem_flg: 0,
        }];
        let mut entries = Vec::new();
        for _ in 0..4 {
            entries.push(enqueue_test_waiter(
                set,
                &ops,
                SemBlockedOp {
                    semnum: 0,
                    wait_type: SemWaitType::Increase,
                    nowait: false,
                },
            ));
        }
        set.remove_waiter(&entries[1]);
        set.remove_waiter(&entries[1]); // An already detached node is harmless.
        let remaining: Vec<_> = set.pending_alter.iter().collect();
        assert_eq!(remaining.len(), 3);
        for (actual, expected) in remaining.iter().zip([0, 2, 3]) {
            assert!(Arc::ptr_eq(actual, &entries[expected]));
        }
        set.remove_waiter(&entries[0]);
        set.remove_waiter(&entries[3]);
        assert!(Arc::ptr_eq(
            set.pending_alter.head.as_ref().unwrap(),
            &entries[2]
        ));
        assert!(Arc::ptr_eq(
            set.pending_alter.tail.as_ref().unwrap(),
            &entries[2]
        ));
        set.remove_waiter(&entries[2]);
        assert!(set.pending_is_empty());
        for entry in entries {
            assert!(entry.complete(Err(SystemError::EINTR)));
        }
    }

    #[test]
    fn last_owner_replays_adjustment_with_clamp_and_removes_record() {
        let ipc_ns = test_ipc_ns();
        let semid = {
            let mut manager = ipc_ns.sem.lock();
            insert_test_set(&mut manager, SemKey::new(31), &[32766])
        };
        let (pcb, group) = test_pcb_with_group(&ipc_ns);
        group.insert_test_record(semid, &[4]);

        detach_sem_undo(&pcb);

        let manager = ipc_ns.sem.lock();
        assert_eq!(sem_values(&manager, semid), vec![SEMVMX]);
        assert_eq!(group.record_count_for_test(), 0);
        assert!(pcb.sem_undo_group().is_none());
    }

    #[test]
    fn non_last_owner_does_not_replay() {
        let ipc_ns = test_ipc_ns();
        let semid = {
            let mut manager = ipc_ns.sem.lock();
            insert_test_set(&mut manager, SemKey::new(32), &[10])
        };
        let (owner_one, group) = test_pcb_with_group(&ipc_ns);
        let owner_two = ProcessControlBlock::new_idle(0, KernelStack::new().unwrap());
        let mut guard = owner_one
            .prepare_shared_sem_undo_attachment(&ipc_ns)
            .unwrap();
        guard.install_into(&owner_two);
        guard.disarm();
        group.insert_test_record(semid, &[4]);

        detach_sem_undo(&owner_one);

        assert_eq!(sem_values(&ipc_ns.sem.lock(), semid), vec![10]);
        assert_eq!(group.record_count_for_test(), 1);

        detach_sem_undo(&owner_two);

        assert_eq!(sem_values(&ipc_ns.sem.lock(), semid), vec![14]);
        assert_eq!(group.record_count_for_test(), 0);
    }

    #[test]
    fn stale_full_semid_does_not_touch_reused_index() {
        let ipc_ns = test_ipc_ns();
        let old_semid = {
            let mut manager = ipc_ns.sem.lock();
            manager.id_allocator = IpcIdAllocator::new(2).unwrap();
            insert_test_set(&mut manager, SemKey::new(33), &[7])
        };
        let (pcb, group) = test_pcb_with_group(&ipc_ns);
        group.insert_test_record(old_semid, &[9]);

        let new_semid = {
            let mut manager = ipc_ns.sem.lock();
            insert_test_set(&mut manager, SemKey::new(34), &[5]);
            remove_test_set(&mut manager, old_semid);
            insert_test_set(&mut manager, SemKey::new(35), &[21])
        };
        assert_ne!(old_semid, new_semid);
        assert_eq!(
            old_semid.data() & IpcIdAllocator::IPC_ID_IDX_MASK,
            new_semid.data() & IpcIdAllocator::IPC_ID_IDX_MASK
        );

        detach_sem_undo(&pcb);

        assert_eq!(sem_values(&ipc_ns.sem.lock(), new_semid), vec![21]);
        assert_eq!(group.record_count_for_test(), 0);
    }

    #[test]
    fn replay_updates_otime_and_rescans_waiter() {
        let ipc_ns = test_ipc_ns();
        let (semid, entry) = {
            let mut manager = ipc_ns.sem.lock();
            let semid = insert_test_set(&mut manager, SemKey::new(35), &[1, 2]);
            let set = manager.get_by_semid_checked_mut(semid).unwrap();
            set.sem_otime = -1;

            let (_waiter, waker) = Waiter::new_pair();
            let blocker = SemBlockedOp {
                semnum: 0,
                wait_type: SemWaitType::Zero,
                nowait: false,
            };
            let entry = Arc::new(SemQueueEntry::new(
                &[PosixSemBuf {
                    sem_num: 0,
                    sem_op: 0,
                    sem_flg: 0,
                }],
                None,
                waker,
                blocker,
            ));
            set.enqueue_waiter(entry.clone());
            (semid, entry)
        };
        let (pcb, group) = test_pcb_with_group(&ipc_ns);
        let exiting_tgid = Pid::new_for_test(RawPid::new(4242), INIT_PID_NAMESPACE.clone());
        pcb.install_pid_identity_for_test(exiting_tgid.clone());
        group.insert_test_record(semid, &[-1, 1]);

        detach_sem_undo(&pcb);

        let mut manager = ipc_ns.sem.lock();
        let set = manager.get_by_semid_checked_mut(semid).unwrap();
        let values = [set.sems[0].val, set.sems[1].val];
        let waiter_pid_was_applied = set.sems[0].pid.is_none();
        let replay_pid_was_applied = set.sems[1]
            .pid
            .as_ref()
            .is_some_and(|pid| Arc::ptr_eq(pid, &exiting_tgid));
        let replay_pid_vnr = set.sems[1]
            .pid
            .as_ref()
            .map(|pid| pid.pid_nr_ns(&INIT_PID_NAMESPACE));
        let sem_otime = set.sem_otime;
        let waiters_are_empty = set.pending_is_empty();
        let completed_result = entry.completed_result();
        let record_count = group.record_count_for_test();

        for sem in &mut set.sems {
            sem.pid = None;
        }
        drop(manager);
        pcb.clear_pid_identity_for_test();
        exiting_tgid.clear_numbers_for_test();

        assert_eq!(values, [0, 3]);
        assert!(waiter_pid_was_applied);
        assert!(replay_pid_was_applied);
        assert_eq!(replay_pid_vnr, Some(RawPid::new(4242)));
        assert_ne!(sem_otime, -1);
        assert!(waiters_are_empty);
        assert_eq!(completed_result, Some(Ok(0)));
        assert_eq!(record_count, 0);
    }

    #[test]
    fn replay_rescans_and_updates_otime_when_clamp_does_not_change_value() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(36), &[SEMVMX, SEMVMX]);
        let entry = {
            let set = manager.get_by_semid_checked_mut(semid).unwrap();
            set.sem_otime = -1;
            enqueue_test_waiter(
                set,
                &[PosixSemBuf {
                    sem_num: 0,
                    sem_op: -1,
                    sem_flg: 0,
                }],
                SemBlockedOp {
                    semnum: 0,
                    wait_type: SemWaitType::Increase,
                    nowait: false,
                },
            )
        };
        let exiting_tgid = Pid::new_for_test(RawPid::new(4243), INIT_PID_NAMESPACE.clone());

        manager.replay_sem_undo_adjustments(
            semid,
            &[1, 1],
            Some(exiting_tgid.clone()),
            &mut SemWakeBatch::default(),
        );

        let set = manager.get_by_semid_checked_mut(semid).unwrap();
        let values = [set.sems[0].val, set.sems[1].val];
        let replay_pid_was_applied = set.sems[1]
            .pid
            .as_ref()
            .is_some_and(|pid| Arc::ptr_eq(pid, &exiting_tgid));
        let sem_otime = set.sem_otime;
        let waiters_are_empty = set.pending_is_empty();
        let completed_result = entry.completed_result();
        for sem in &mut set.sems {
            sem.pid = None;
        }
        exiting_tgid.clear_numbers_for_test();

        assert_eq!(values, [SEMVMX - 1, SEMVMX]);
        assert!(replay_pid_was_applied);
        assert_ne!(sem_otime, -1);
        assert!(waiters_are_empty);
        assert_eq!(completed_result, Some(Ok(0)));
    }

    #[test]
    fn valid_all_zero_record_still_updates_otime_and_rescans_queue() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(37), &[0, 5]);
        let entry = {
            let set = manager.get_by_semid_checked_mut(semid).unwrap();
            set.sem_otime = -1;
            enqueue_test_waiter(
                set,
                &[PosixSemBuf {
                    sem_num: 0,
                    sem_op: 0,
                    sem_flg: 0,
                }],
                SemBlockedOp {
                    semnum: 0,
                    wait_type: SemWaitType::Zero,
                    nowait: false,
                },
            )
        };
        let exiting_tgid = Pid::new_for_test(RawPid::new(4244), INIT_PID_NAMESPACE.clone());

        manager.replay_sem_undo_adjustments(
            semid,
            &[0, 0],
            Some(exiting_tgid.clone()),
            &mut SemWakeBatch::default(),
        );

        let set = manager.get_by_semid_checked_mut(semid).unwrap();
        let values = [set.sems[0].val, set.sems[1].val];
        let untouched_pid = set.sems[1].pid.is_none();
        let sem_otime = set.sem_otime;
        let waiters_are_empty = set.pending_is_empty();
        let completed_result = entry.completed_result();
        for sem in &mut set.sems {
            sem.pid = None;
        }
        exiting_tgid.clear_numbers_for_test();

        assert_eq!(values, [0, 5]);
        assert!(untouched_pid);
        assert_ne!(sem_otime, -1);
        assert!(waiters_are_empty);
        assert_eq!(completed_result, Some(Ok(0)));
    }

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
            assert_eq!(manager.get_by_semid_checked(*id).unwrap().sems[0].val, 3);
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
    fn undo_registry_shrink_releases_empty_and_rechecks_concurrent_growth() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(152), &[0]);
        let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        let set = manager.get_by_semid_checked_mut(semid).unwrap();
        set.undo_groups.reserve_exact(64);
        set.undo_groups.push(Arc::downgrade(&group));
        let mut spare = Vec::new();
        let mut retired = Vec::new();
        let needed = set.shrink_undo_registry_prepared(&mut spare, &mut retired);
        assert_eq!(needed, 4);
        spare.try_reserve_exact(needed).unwrap();
        // Simulate new associations arriving during unlocked preparation.
        for _ in 0..8 {
            set.undo_groups.push(Arc::downgrade(&group));
        }
        assert!(set.shrink_undo_registry_prepared(&mut spare, &mut retired) > 0);
        assert_eq!(set.undo_groups.len(), 9);
        assert_eq!(set.undo_groups.capacity(), 64);
        set.undo_groups.truncate(1);
        assert_eq!(
            set.shrink_undo_registry_prepared(&mut spare, &mut retired),
            0
        );
        assert_eq!(set.undo_groups.len(), 1);
        assert_eq!(set.undo_groups.capacity(), 4);
        assert_eq!(spare.capacity(), 64);
        // The spare is already allocated when another owner empties the set.
        // It must not be installed back into the empty registry.
        set.undo_groups.clear();
        assert_eq!(
            set.shrink_undo_registry_prepared(&mut spare, &mut retired),
            0
        );
        assert_eq!(set.undo_groups.capacity(), 0);
        assert_eq!(retired.capacity(), 4);
    }

    #[test]
    fn prepared_registry_growth_rechecks_registration_and_capacity() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(151), &[1]);
        let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        let mut spare = Vec::new();
        let capacity = manager
            .get_by_semid_checked_mut(semid)
            .unwrap()
            .ensure_undo_group_registered_prepared(&group, &mut spare)
            .unwrap_err();
        assert!(!manager.undo_registry_contains_for_test(&group));
        assert!(manager
            .get_by_semid_checked(semid)
            .unwrap()
            .undo_groups
            .is_empty());
        spare.try_reserve(capacity).unwrap();

        // Simulate other first-time groups consuming the prepared capacity
        // while this caller has dropped the manager lock.
        let mut owners = Vec::new();
        while manager
            .get_by_semid_checked(semid)
            .unwrap()
            .undo_groups
            .len()
            < spare.capacity()
        {
            let owner = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
            manager.ensure_undo_group_registered(&owner, semid).unwrap();
            owners.push(owner);
        }
        let capacity = manager
            .get_by_semid_checked_mut(semid)
            .unwrap()
            .ensure_undo_group_registered_prepared(&group, &mut spare)
            .unwrap_err();
        assert!(!manager.undo_registry_contains_for_test(&group));
        spare.try_reserve(capacity).unwrap();
        manager
            .get_by_semid_checked_mut(semid)
            .unwrap()
            .ensure_undo_group_registered_prepared(&group, &mut spare)
            .unwrap();
        assert!(spare.is_empty());
        assert!(manager.undo_registry_contains_for_test(&group));
        assert_eq!(
            manager
                .get_by_semid_checked(semid)
                .unwrap()
                .undo_groups
                .len(),
            owners.len() + 1
        );
        for (weak, owner) in manager
            .get_by_semid_checked(semid)
            .unwrap()
            .undo_groups
            .iter()
            .zip(&owners)
        {
            assert!(weak.ptr_eq(&Arc::downgrade(owner)));
        }

        // A concurrent CLONE_SYSVSEM sharer already registered this group.
        let mut empty_spare = Vec::new();
        manager
            .get_by_semid_checked_mut(semid)
            .unwrap()
            .ensure_undo_group_registered_prepared(&group, &mut empty_spare)
            .unwrap();
        assert_eq!(
            manager
                .get_by_semid_checked(semid)
                .unwrap()
                .undo_groups
                .len(),
            owners.len() + 1
        );
    }

    #[test]
    fn queued_undo_commits_to_captured_group_not_waker_current_task() {
        let ipc_ns = test_ipc_ns();
        let group_a = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
        let group_b = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
        let mut manager = ipc_ns.sem.lock();
        let semid = insert_test_set(&mut manager, SemKey::new(46), &[0]);
        manager
            .prepare_undo_record_and_registry_for_test(&group_a, semid)
            .unwrap();
        manager
            .prepare_undo_record_and_registry_for_test(&group_b, semid)
            .unwrap();

        let (_waiter, waker) = Waiter::new_pair();
        let entry = Arc::new(SemQueueEntry::new_prepared(
            SemQueueEntry::prepare_sops(&[undo_sop(0, -1)]).unwrap(),
            None,
            Some(group_a.clone()),
            Some(group_a.prepare_record_for_test(semid, 1).unwrap()),
            waker,
            SemopScratch::try_new(1).unwrap(),
            SemBlockedOp {
                semnum: 0,
                wait_type: SemWaitType::Increase,
                nowait: false,
            },
        ));
        let set = manager.get_by_semid_checked_mut(semid).unwrap();
        set.enqueue_waiter(entry.clone());
        set.sems[0].val = 1;

        manager.update_queue_for_test(semid);

        assert_eq!(entry.completed_result(), Some(Ok(0)));
        assert_eq!(sem_values(&manager, semid), vec![0]);
        assert_eq!(group_a.adjustment_for_test(semid, 0), 1);
        assert_eq!(group_b.adjustment_for_test(semid, 0), 0);
    }

    #[test]
    fn queued_timeout_signal_and_rmid_never_commit_adjustment() {
        let ipc_ns = test_ipc_ns();
        let group = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
        let mut manager = ipc_ns.sem.lock();
        let timeout_semid = insert_test_set(&mut manager, SemKey::new(47), &[0]);
        let signal_semid = insert_test_set(&mut manager, SemKey::new(48), &[0]);
        let rmid_semid = insert_test_set(&mut manager, SemKey::new(49), &[0]);
        for semid in [timeout_semid, signal_semid, rmid_semid] {
            manager
                .prepare_undo_record_and_registry_for_test(&group, semid)
                .unwrap();
        }

        let timeout_entry = enqueue_undo_waiter_for_test(&mut manager, timeout_semid, &group);
        assert_eq!(
            manager.cancel_queued_entry(
                timeout_semid,
                &timeout_entry,
                SystemError::EAGAIN_OR_EWOULDBLOCK,
            ),
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        );
        assert_eq!(group.adjustment_for_test(timeout_semid, 0), 0);

        let signal_entry = enqueue_undo_waiter_for_test(&mut manager, signal_semid, &group);
        assert_eq!(
            manager.cancel_queued_entry(signal_semid, &signal_entry, SystemError::EINTR),
            Err(SystemError::EINTR)
        );
        assert_eq!(group.adjustment_for_test(signal_semid, 0), 0);

        let rmid_entry = enqueue_undo_waiter_for_test(&mut manager, rmid_semid, &group);
        manager
            .ipc_rmid(rmid_semid, &mut SemWakeBatch::default())
            .unwrap();
        assert_eq!(rmid_entry.completed_result(), Some(Err(SystemError::EIDRM)));
        assert_eq!(group.adjustment_for_test(rmid_semid, 0), 0);
    }

    #[test]
    fn first_record_is_registry_visible_before_future_cleanup() {
        let ipc_ns = test_ipc_ns();
        let group = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
        let mut manager = ipc_ns.sem.lock();
        let semid = insert_test_set(&mut manager, SemKey::new(50), &[3]);

        manager
            .prepare_undo_record_and_registry_for_test(&group, semid)
            .unwrap();

        assert_eq!(manager.live_undo_group_count_for_test(), 1);
        assert!(manager.undo_registry_contains_for_test(&group));
        assert_eq!(group.adjustment_for_test(semid, 0), 0);
    }

    #[test]
    fn stale_weak_entries_are_compacted_without_losing_live_group() {
        let ipc_ns = test_ipc_ns();
        let live = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
        let candidate = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
        let stale = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
        let stale_weak = Arc::downgrade(&stale);
        drop(stale);

        let mut manager = ipc_ns.sem.lock();
        let semid = insert_test_set(&mut manager, SemKey::new(51), &[1]);
        manager
            .get_by_semid_checked_mut(semid)
            .unwrap()
            .undo_groups
            .push(stale_weak);
        manager.ensure_undo_group_registered(&live, semid).unwrap();
        manager
            .get_by_semid_checked_mut(semid)
            .unwrap()
            .undo_groups
            .shrink_to_fit();

        manager
            .prepare_undo_record_and_registry_for_test(&candidate, semid)
            .unwrap();

        assert_eq!(manager.live_undo_group_count_for_test(), 2);
        assert!(manager.undo_registry_contains_for_test(&live));
        assert!(manager.undo_registry_contains_for_test(&candidate));
    }

    #[test]
    fn live_group_registration_survives_debt_removal_without_duplicates() {
        let ipc_ns = test_ipc_ns();
        let group = SemUndoGroup::new_for_test_bound_to(&ipc_ns).unwrap();
        let mut manager = ipc_ns.sem.lock();
        let semid = insert_test_set(&mut manager, SemKey::new(150), &[1]);
        manager
            .prepare_undo_record_and_registry_for_test(&group, semid)
            .unwrap();
        let capacity = manager
            .get_by_semid_checked(semid)
            .unwrap()
            .undo_groups
            .capacity();
        manager.clear_undo_for_setall(semid);
        manager.discard_undo_for_rmid(semid);
        assert_eq!(group.record_count_for_test(), 0);
        for _ in 0..32 {
            manager.ensure_undo_group_registered(&group, semid).unwrap();
        }
        assert!(manager.undo_registry_contains_for_test(&group));
        assert_eq!(
            manager
                .get_by_semid_checked(semid)
                .unwrap()
                .undo_groups
                .len(),
            1
        );
        assert_eq!(
            manager
                .get_by_semid_checked(semid)
                .unwrap()
                .undo_groups
                .capacity(),
            capacity
        );
        manager
            .prepare_undo_record_and_registry_for_test(&group, semid)
            .unwrap();
        assert_eq!(
            manager
                .get_by_semid_checked(semid)
                .unwrap()
                .undo_groups
                .len(),
            1
        );
    }

    #[test]
    fn queued_undo_entry_retains_group_after_external_owner_drops() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(52), &[1]);
        let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        manager
            .prepare_undo_record_and_registry_for_test(&group, semid)
            .unwrap();
        let entry = enqueue_undo_waiter_for_test(&mut manager, semid, &group);
        let group_weak = Arc::downgrade(&group);
        drop(group);
        assert!(group_weak.upgrade().is_some());
        manager.get_by_semid_checked_mut(semid).unwrap().sems[0].val = 1;

        manager.update_queue_for_test(semid);

        assert_eq!(entry.completed_result(), Some(Ok(0)));
        assert_eq!(sem_values(&manager, semid), vec![0]);
    }

    #[test]
    fn queued_undo_record_length_mismatch_completes_with_internal_error() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(53), &[1, 1]);
        let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        group.insert_test_record(semid, &[0]);
        let entry = enqueue_undo_waiter_for_test(&mut manager, semid, &group);
        manager.get_by_semid_checked_mut(semid).unwrap().sems[0].val = 1;

        manager.update_queue_for_test(semid);

        assert_eq!(entry.completed_result(), Some(Err(SystemError::EINVAL)));
        assert_eq!(sem_values(&manager, semid), vec![1, 1]);
        assert_eq!(group.adjustment_for_test(semid, 0), 0);
    }

    #[test]
    fn final_owner_detach_replays_against_setval_as_a_single_serial_order() {
        let ipc_ns = test_ipc_ns();
        let semid = {
            let mut manager = ipc_ns.sem.lock();
            insert_test_set(&mut manager, SemKey::new(62), &[2])
        };
        let (pcb, group) = test_pcb_with_group(&ipc_ns);
        group.insert_test_record(semid, &[3]);
        ipc_ns
            .sem
            .lock()
            .ensure_undo_group_registered(&group, semid)
            .unwrap();
        drop(pcb.take_sem_undo_attachment().unwrap());
        assert!(group.detach_last_owner_for_test());

        {
            let mut manager = ipc_ns.sem.lock();
            manager
                .setval(semid, 0, 7, &mut SemWakeBatch::default())
                .unwrap();
        }
        group.replay_marked_records_for_test(&pcb);

        let manager = ipc_ns.sem.lock();
        assert_eq!(sem_values(&manager, semid), vec![7]);
        assert_eq!(group.record_count_for_test(), 0);
        assert_eq!(group.replay_count_for_test(), 1);
        assert!(pcb.sem_undo_group().is_none());
    }

    #[test]
    fn rmid_before_final_owner_replay_skips_detached_record_once() {
        let ipc_ns = test_ipc_ns();
        let semid = {
            let mut manager = ipc_ns.sem.lock();
            insert_test_set(&mut manager, SemKey::new(63), &[2])
        };
        let (pcb, group) = test_pcb_with_group(&ipc_ns);
        group.insert_test_record(semid, &[3]);
        ipc_ns
            .sem
            .lock()
            .ensure_undo_group_registered(&group, semid)
            .unwrap();
        drop(pcb.take_sem_undo_attachment().unwrap());
        assert!(group.detach_last_owner_for_test());

        {
            let mut manager = ipc_ns.sem.lock();
            manager
                .ipc_rmid(semid, &mut SemWakeBatch::default())
                .unwrap();
        }
        group.replay_marked_records_for_test(&pcb);

        let manager = ipc_ns.sem.lock();
        assert_eq!(
            manager.get_by_semid_checked(semid),
            Err(SystemError::EINVAL)
        );
        assert_eq!(group.record_count_for_test(), 0);
        assert_eq!(group.replay_count_for_test(), 1);
        assert!(pcb.sem_undo_group().is_none());
    }

    #[test]
    fn prepare_existing_undo_record_length_mismatch_returns_einval_without_mutation() {
        let semid = SemId::new(64);
        let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        group.insert_test_record(semid, &[5]);

        let err = group.prepare_record_for_test(semid, 2).unwrap_err();

        assert_eq!(err, SystemError::EINVAL);
        assert_eq!(group.adjustment_for_test(semid, 0), 5);
        assert_eq!(group.record_count_for_test(), 1);
        assert_eq!(group.pending_record_reservations_for_test(), 0);
    }

    #[test]
    fn setval_clears_only_target_sem_adjustment_across_all_groups() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(54), &[4, 5]);
        let other_semid = insert_test_set(&mut manager, SemKey::new(55), &[6, 7]);
        let group_a = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        let group_b = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        group_a.insert_test_record(semid, &[11, 12]);
        group_b.insert_test_record(semid, &[21, 22]);
        group_a.insert_test_record(other_semid, &[31, 32]);
        manager
            .ensure_undo_group_registered(&group_a, semid)
            .unwrap();
        manager
            .ensure_undo_group_registered(&group_b, semid)
            .unwrap();

        manager
            .setval(semid, 0, 9, &mut SemWakeBatch::default())
            .unwrap();

        assert_eq!(sem_values(&manager, semid), vec![9, 5]);
        assert_eq!(group_a.adjustment_for_test(semid, 0), 0);
        assert_eq!(group_a.adjustment_for_test(semid, 1), 12);
        assert_eq!(group_b.adjustment_for_test(semid, 0), 0);
        assert_eq!(group_b.adjustment_for_test(semid, 1), 22);
        assert_eq!(group_a.adjustment_for_test(other_semid, 0), 31);
    }

    #[test]
    fn setall_clears_entire_full_semid_record_across_all_groups() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(56), &[1, 2, 3]);
        let other_semid = insert_test_set(&mut manager, SemKey::new(57), &[4, 5, 6]);
        let group_a = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        let group_b = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        group_a.insert_test_record(semid, &[1, 2, 3]);
        group_b.insert_test_record(semid, &[4, 5, 6]);
        group_b.insert_test_record(other_semid, &[7, 8, 9]);
        manager
            .ensure_undo_group_registered(&group_a, semid)
            .unwrap();
        manager
            .ensure_undo_group_registered(&group_b, semid)
            .unwrap();
        let token = SemSetAllToken::new(semid, 3);

        manager
            .setall(token, &[10, 11, 12], &mut SemWakeBatch::default())
            .unwrap();

        assert_eq!(sem_values(&manager, semid), vec![10, 11, 12]);
        assert_eq!(group_a.adjustment_for_test(semid, 0), 0);
        assert_eq!(group_a.adjustment_for_test(semid, 1), 0);
        assert_eq!(group_a.adjustment_for_test(semid, 2), 0);
        assert_eq!(group_b.adjustment_for_test(semid, 0), 0);
        assert_eq!(group_b.adjustment_for_test(semid, 1), 0);
        assert_eq!(group_b.adjustment_for_test(semid, 2), 0);
        assert_eq!(group_b.adjustment_for_test(other_semid, 1), 8);
    }

    #[test]
    fn setval_cleanup_precedes_value_write_and_queue_rescan() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(58), &[0]);
        let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        group.insert_test_record(semid, &[7]);
        manager.ensure_undo_group_registered(&group, semid).unwrap();
        let entry = enqueue_undo_waiter_for_test(&mut manager, semid, &group);

        manager
            .setval(semid, 0, 1, &mut SemWakeBatch::default())
            .unwrap();

        assert_eq!(entry.completed_result(), Some(Ok(0)));
        assert_eq!(sem_values(&manager, semid), vec![0]);
        assert_eq!(group.adjustment_for_test(semid, 0), 1);
    }

    #[test]
    fn rmid_discards_record_before_index_can_be_reused() {
        let mut manager = SemManager::new();
        manager.id_allocator = IpcIdAllocator::new(2).unwrap();
        let old_semid = insert_test_set(&mut manager, SemKey::new(59), &[3]);
        let filler_semid = insert_test_set(&mut manager, SemKey::new(60), &[4]);
        let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        group.insert_test_record(old_semid, &[9]);
        manager
            .ensure_undo_group_registered(&group, old_semid)
            .unwrap();
        manager
            .ipc_rmid(old_semid, &mut SemWakeBatch::default())
            .unwrap();

        let new_semid = insert_test_set(&mut manager, SemKey::new(61), &[5]);

        assert_ne!(old_semid, new_semid);
        assert_eq!(
            old_semid.data() & IpcIdAllocator::IPC_ID_IDX_MASK,
            new_semid.data() & IpcIdAllocator::IPC_ID_IDX_MASK
        );
        assert_eq!(sem_values(&manager, new_semid), vec![5]);
        assert_eq!(group.record_count_for_test(), 0);
        assert_eq!(sem_values(&manager, filler_semid), vec![4]);
    }

    #[test]
    fn setval_clear_between_prepare_and_commit_refreshes_stale_existing_record() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(62), &[4]);
        let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        group.insert_test_record(semid, &[7]);
        manager.ensure_undo_group_registered(&group, semid).unwrap();

        let record = group.prepare_record_for_test(semid, 1).unwrap();
        manager.clear_undo_for_setval(semid, 0);
        let mut scratch = SemopScratch::try_new(1).unwrap();
        let set = manager.get_by_semid_checked_mut(semid).unwrap();

        let result = group.with_prepared_record_noalloc(record, |record| {
            let outcome =
                SemManager::simulate_semop(set, &[undo_sop(0, -1)], Some(record), &mut scratch)
                    .unwrap()
                    .ready_for_test();
            SemManager::commit_semop(set, outcome, &scratch, None, Some(record));
            PreparedSemUndoRecordAction::Publish(())
        });

        assert!(result.is_ok());
        assert_eq!(sem_values(&manager, semid), vec![3]);
        assert_eq!(group.adjustment_for_test(semid, 0), 1);
    }

    #[test]
    fn setall_clear_between_prepare_and_commit_refreshes_stale_existing_record() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(63), &[4, 5]);
        let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        group.insert_test_record(semid, &[7, -3]);
        manager.ensure_undo_group_registered(&group, semid).unwrap();

        let record = group.prepare_record_for_test(semid, 2).unwrap();
        manager.clear_undo_for_setall(semid);
        let mut scratch = SemopScratch::try_new(1).unwrap();
        let set = manager.get_by_semid_checked_mut(semid).unwrap();

        let result = group.with_prepared_record_noalloc(record, |record| {
            let outcome =
                SemManager::simulate_semop(set, &[undo_sop(0, -1)], Some(record), &mut scratch)
                    .unwrap()
                    .ready_for_test();
            SemManager::commit_semop(set, outcome, &scratch, None, Some(record));
            PreparedSemUndoRecordAction::Publish(())
        });

        assert!(result.is_ok());
        assert_eq!(sem_values(&manager, semid), vec![3, 5]);
        assert_eq!(group.adjustment_for_test(semid, 0), 1);
        assert_eq!(group.adjustment_for_test(semid, 1), 0);
    }

    #[test]
    fn stale_existing_prepared_record_refreshes_before_immediate_commit() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(65), &[4]);
        let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        group.insert_test_record(semid, &[7]);
        manager.ensure_undo_group_registered(&group, semid).unwrap();

        let record = group.prepare_record_for_test(semid, 1).unwrap();
        manager.clear_undo_for_setval(semid, 0);
        let mut scratch = SemopScratch::try_new(1).unwrap();
        let set = manager.get_by_semid_checked_mut(semid).unwrap();

        let result = group.with_prepared_record_noalloc(record, |record| {
            let outcome =
                SemManager::simulate_semop(set, &[undo_sop(0, -1)], Some(record), &mut scratch)
                    .unwrap()
                    .ready_for_test();
            SemManager::commit_semop(set, outcome, &scratch, None, Some(record));
            PreparedSemUndoRecordAction::Publish(())
        });

        assert!(result.is_ok());
        assert_eq!(sem_values(&manager, semid), vec![3]);
        assert_eq!(group.adjustment_for_test(semid, 0), 1);
    }

    #[test]
    fn queued_stale_existing_record_retries_and_completes_without_error() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(66), &[0]);
        let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        group.insert_test_record(semid, &[7]);
        manager.ensure_undo_group_registered(&group, semid).unwrap();
        let entry = enqueue_undo_waiter_for_test(&mut manager, semid, &group);

        manager.clear_undo_for_setval(semid, 0);
        manager.get_by_semid_checked_mut(semid).unwrap().sems[0].val = 2;
        manager.update_queue_for_test(semid);

        assert_eq!(entry.completed_result(), Some(Ok(0)));
        assert_eq!(sem_values(&manager, semid), vec![1]);
        assert_eq!(group.adjustment_for_test(semid, 0), 1);
    }

    #[test]
    fn consecutive_sem_undo_on_unchanged_existing_record_still_accumulates() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(64), &[5]);
        let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        group.insert_test_record(semid, &[2]);

        let mut record = group.prepare_record_for_test(semid, 1).unwrap();
        let mut scratch = SemopScratch::try_new(1).unwrap();
        let set = manager.get_by_semid_checked_mut(semid).unwrap();
        let outcome =
            SemManager::simulate_semop(set, &[undo_sop(0, -2)], Some(&mut record), &mut scratch)
                .unwrap()
                .ready_for_test();
        SemManager::commit_semop(set, outcome, &scratch, None, Some(&mut record));
        group.commit_prepared_record_noalloc(record).unwrap();

        assert_eq!(group.adjustment_for_test(semid, 0), 4);
    }

    fn enqueue_undo_waiter_for_test(
        manager: &mut SemManager,
        semid: SemId,
        group: &Arc<SemUndoGroup>,
    ) -> Arc<SemQueueEntry> {
        let (_waiter, waker) = Waiter::new_pair();
        let entry = Arc::new(SemQueueEntry::new_prepared(
            SemQueueEntry::prepare_sops(&[undo_sop(0, -1)]).unwrap(),
            None,
            Some(Arc::clone(group)),
            Some(group.prepare_record_for_test(semid, 1).unwrap()),
            waker,
            SemopScratch::try_new(1).unwrap(),
            SemBlockedOp {
                semnum: 0,
                wait_type: SemWaitType::Increase,
                nowait: false,
            },
        ));
        manager
            .get_by_semid_checked_mut(semid)
            .unwrap()
            .enqueue_waiter(entry.clone());
        entry
    }

    fn undo_sop(sem_num: u16, sem_op: i16) -> PosixSemBuf {
        PosixSemBuf {
            sem_num,
            sem_op,
            sem_flg: SemFlags::SEM_UNDO.bits() as i16,
        }
    }

    fn plain_sop(sem_num: u16, sem_op: i16) -> PosixSemBuf {
        PosixSemBuf {
            sem_num,
            sem_op,
            sem_flg: 0,
        }
    }

    fn nowait_sop(sem_num: u16, sem_op: i16) -> PosixSemBuf {
        PosixSemBuf {
            sem_num,
            sem_op,
            sem_flg: SemFlags::IPC_NOWAIT.bits() as i16,
        }
    }

    #[test]
    fn const_waiters_complete_before_altering_waiters() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(67), &[1]);
        let (altering, constant) = {
            let set = manager.get_by_semid_checked_mut(semid).unwrap();
            let altering = enqueue_test_waiter(
                set,
                &[plain_sop(0, 0), plain_sop(0, 1)],
                SemBlockedOp {
                    semnum: 0,
                    wait_type: SemWaitType::Zero,
                    nowait: false,
                },
            );
            let constant = enqueue_test_waiter(
                set,
                &[plain_sop(0, 0)],
                SemBlockedOp {
                    semnum: 0,
                    wait_type: SemWaitType::Zero,
                    nowait: false,
                },
            );
            set.sems[0].val = 0;
            (altering, constant)
        };

        manager.update_queue_for_test(semid);

        let set = manager.get_by_semid_checked(semid).unwrap();
        assert_eq!(constant.completed_result(), Some(Ok(0)));
        assert_eq!(altering.completed_result(), Some(Ok(0)));
        assert!(set.pending_is_empty());
        assert_eq!(set.sems[0].val, 1);
    }

    #[test]
    fn ordered_mixed_undo_ops_apply_each_adjustment_step() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(41), &[4]);
        let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        let mut record = group.prepare_record_for_test(semid, 1).unwrap();
        let mut scratch = SemopScratch::try_new(3).unwrap();
        let sops = [undo_sop(0, 3), plain_sop(0, -1), undo_sop(0, -2)];

        let set = manager.get_by_semid_checked_mut(semid).unwrap();
        let outcome = SemManager::simulate_semop(set, &sops, Some(&mut record), &mut scratch);
        assert!(matches!(outcome, Ok(SemopOutcome::Ready(_))));
        SemManager::commit_semop(
            set,
            outcome.unwrap().ready_for_test(),
            &scratch,
            None,
            Some(&mut record),
        );

        assert_eq!(set.sems[0].val, 4);
        assert_eq!(record.adjustment_for_test(0), -1);
    }

    #[test]
    fn intermediate_adjustment_overflow_is_erange_even_if_later_op_cancels_it() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(42), &[10]);
        let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        let mut record = group.prepare_record_for_test(semid, 1).unwrap();
        record.set_adjustment_for_test(0, i16::MAX);
        let mut scratch = SemopScratch::try_new(2).unwrap();
        let sops = [undo_sop(0, -1), undo_sop(0, 1)];

        let set = manager.get_by_semid_checked_mut(semid).unwrap();
        assert!(matches!(
            SemManager::simulate_semop(set, &sops, Some(&mut record), &mut scratch),
            Err(SystemError::ERANGE)
        ));

        assert_eq!(set.sems[0].val, 10);
        assert_eq!(record.adjustment_for_test(0), i16::MAX);
    }

    #[test]
    fn blocked_or_nowait_failure_does_not_commit_semval_or_semadj_prefix() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(43), &[2]);
        let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        let mut record = group.prepare_record_for_test(semid, 1).unwrap();
        let mut scratch = SemopScratch::try_new(2).unwrap();
        let sops = [undo_sop(0, -1), nowait_sop(0, -2)];

        let set = manager.get_by_semid_checked_mut(semid).unwrap();
        assert!(matches!(
            SemManager::simulate_semop(set, &sops, Some(&mut record), &mut scratch),
            Ok(SemopOutcome::Blocked(SemBlockedOp {
                semnum: 0,
                wait_type: SemWaitType::Increase,
                nowait: true,
            }))
        ));

        assert_eq!(set.sems[0].val, 2);
        assert_eq!(record.adjustment_for_test(0), 0);
    }

    #[test]
    fn zero_undo_op_can_prepare_zero_record_without_adjustment() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(44), &[0]);
        let group = SemUndoGroup::new_for_test_bound_to(&INIT_IPC_NAMESPACE).unwrap();
        let mut record = group.prepare_record_for_test(semid, 1).unwrap();
        let mut scratch = SemopScratch::try_new(1).unwrap();
        let sops = [undo_sop(0, 0)];

        let set = manager.get_by_semid_checked_mut(semid).unwrap();
        let outcome = SemManager::simulate_semop(set, &sops, Some(&mut record), &mut scratch);
        assert!(matches!(outcome, Ok(SemopOutcome::Ready(_))));
        SemManager::commit_semop(
            set,
            outcome.unwrap().ready_for_test(),
            &scratch,
            None,
            Some(&mut record),
        );
        group.commit_record(record).unwrap();

        assert_eq!(set.sems[0].val, 0);
        assert_eq!(group.record_count_for_test(), 1);
    }

    #[test]
    fn scratch_entry_for_returns_enomem_instead_of_extending_past_capacity() {
        let mut manager = SemManager::new();
        let semid = insert_test_set(&mut manager, SemKey::new(45), &[1, 1]);
        let set = manager.get_by_semid_checked_mut(semid).unwrap();
        let mut scratch = SemopScratch::try_new(1).unwrap();
        let sops = [plain_sop(0, -1), plain_sop(1, -1)];

        assert!(matches!(
            SemManager::simulate_semop(set, &sops, None, &mut scratch),
            Err(SystemError::ENOMEM)
        ));
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
}
