//! Intrusive pending queues, cached wait counts and one-way completion publication.
use super::*;
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
pub(super) struct SemWaitQueue {
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
pub(in crate::ipc::sem) struct SemQueueEntry {
    sops: Vec<PosixSemBuf>,
    pid: Option<Arc<Pid>>,
    undo_group: Option<Arc<SemUndoGroup>>,
    pub(in crate::ipc::sem) undo_record: SpinLock<Option<PreparedSemUndoRecord>>,
    waker: Arc<Waker>,
    scratch: SpinLock<SemopScratch>,
    status: SpinLock<SemQueueStatus>,
}

impl SemQueueEntry {
    pub(in crate::ipc::sem) fn new_prepared(
        sops: Vec<PosixSemBuf>,
        pid: Option<Arc<Pid>>,
        undo_group: Option<Arc<SemUndoGroup>>,
        undo_record: Option<PreparedSemUndoRecord>,
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

    pub(in crate::ipc::sem) fn prepare_sops(
        sops: &[PosixSemBuf],
    ) -> Result<Vec<PosixSemBuf>, SystemError> {
        let mut owned_sops = Vec::new();
        owned_sops
            .try_reserve_exact(sops.len())
            .map_err(|_| SystemError::ENOMEM)?;
        owned_sops.extend_from_slice(sops);
        Ok(owned_sops)
    }

    #[cfg(test)]
    pub(in crate::ipc::sem) fn new(
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
            SemopScratch::try_new(sops).unwrap(),
            blocker,
        )
    }

    pub(in crate::ipc::sem) fn completed_result(&self) -> Option<Result<usize, SystemError>> {
        match &*self.status.lock() {
            SemQueueStatus::Queued { .. } => None,
            SemQueueStatus::Completed { result, .. } => Some(result.clone()),
        }
    }

    pub(in crate::ipc::sem) fn complete(&self, result: Result<usize, SystemError>) -> bool {
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

    #[cfg(test)]
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
    fn push_completed(&mut self, entry: Arc<SemQueueEntry>) {
        debug_assert!(entry.completed_result().is_some());
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

impl KernelSemSet {
    fn pending_queue_for(sops: &[PosixSemBuf]) -> SemPendingQueue {
        if sops.iter().any(|op| op.sem_op != 0) {
            SemPendingQueue::Alter
        } else {
            SemPendingQueue::Const
        }
    }

    /// The prepared entry itself owns the queue links; publication cannot allocate.
    pub(in crate::ipc::sem) fn enqueue_waiter(&mut self, waiter: Arc<SemQueueEntry>) {
        let blocker = match &*waiter.status.lock() {
            SemQueueStatus::Queued {
                blocker,
                links: None,
            } => *blocker,
            _ => panic!("enqueue requires an unlinked semaphore operation"),
        };
        let queue = match Self::pending_queue_for(&waiter.sops) {
            SemPendingQueue::Const => &mut self.pending_const,
            SemPendingQueue::Alter => &mut self.pending_alter,
        };
        queue.push_back(waiter);
        self.change_wait_count(blocker, true);
    }

    fn change_wait_count(&mut self, blocker: SemBlockedOp, increase: bool) {
        let sem = &mut self.sems[blocker.semnum];
        let count = match blocker.wait_type {
            SemWaitType::Increase => &mut sem.ncnt,
            SemWaitType::Zero => &mut sem.zcnt,
        };
        *count = if increase {
            count.checked_add(1).expect("semaphore wait count overflow")
        } else {
            count
                .checked_sub(1)
                .expect("semaphore wait count underflow")
        };
    }

    /// Only linked entries contribute to counts; preparation uses this same
    /// operation without accounting. Manager lock serializes both kinds.
    pub(in crate::ipc::sem) fn update_blocker(
        &mut self,
        entry: &SemQueueEntry,
        blocker: SemBlockedOp,
    ) {
        let mut status = entry.status.lock();
        if let SemQueueStatus::Queued {
            blocker: old,
            links,
        } = &mut *status
        {
            if links.is_some()
                && (old.semnum != blocker.semnum || old.wait_type != blocker.wait_type)
            {
                self.change_wait_count(*old, false);
                self.change_wait_count(blocker, true);
            }
            *old = blocker;
        }
    }

    #[cfg(test)]
    fn remove_waiter(&mut self, target: &Arc<SemQueueEntry>) {
        self.remove_pending(Self::pending_queue_for(&target.sops), target);
    }

    #[cfg(test)]
    pub(super) fn pending_is_empty(&self) -> bool {
        self.pending_const.is_empty() && self.pending_alter.is_empty()
    }

    fn pending_iter(&self, queue: SemPendingQueue) -> SemWaitIter {
        match queue {
            SemPendingQueue::Const => self.pending_const.iter(),
            SemPendingQueue::Alter => self.pending_alter.iter(),
        }
    }

    fn remove_pending(&mut self, queue: SemPendingQueue, entry: &Arc<SemQueueEntry>) {
        let blocker = match &*entry.status.lock() {
            SemQueueStatus::Queued {
                blocker,
                links: Some(_),
                ..
            } => *blocker,
            _ => return,
        };
        let removed = match queue {
            SemPendingQueue::Const => self.pending_const.remove(entry),
            SemPendingQueue::Alter => self.pending_alter.remove(entry),
        };
        if removed {
            self.change_wait_count(blocker, false);
        }
    }

    /// Publish removal under the manager lock; the caller wakes after unlocking.
    pub(in crate::ipc::sem) fn complete_all_removed(&mut self, wakes: &mut SemWakeBatch) {
        for entry in self.pending_const.iter() {
            self.finish_and_wake(
                SemPendingQueue::Const,
                entry,
                Err(SystemError::EIDRM),
                wakes,
            );
        }
        for entry in self.pending_alter.iter() {
            self.finish_and_wake(
                SemPendingQueue::Alter,
                entry,
                Err(SystemError::EIDRM),
                wakes,
            );
        }
    }

    #[cfg(test)]
    fn ncnt(&self, semnum: usize) -> usize {
        self.sems[semnum].ncnt
    }

    #[cfg(test)]
    fn zcnt(&self, semnum: usize) -> usize {
        self.sems[semnum].zcnt
    }
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
                        set.finish_and_wake(queue, entry.clone(), Err(SystemError::EINVAL), wakes);
                        continue;
                    };
                    match group.with_prepared_record_noalloc(record, |record| {
                        match Self::retry_queued_undo_entry(set, &entry, record) {
                            Ok(Some(changed)) => {
                                PreparedSemUndoRecordAction::Complete(Ok(Some(changed)))
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
                        set.finish_and_wake(queue, entry.clone(), Ok(0), wakes);
                        if changed {
                            return true;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        set.finish_and_wake(queue, entry.clone(), Err(error), wakes);
                    }
                }
                continue;
            }

            let mut scratch = entry.scratch.lock();
            match set.try_apply(&entry.sops, entry.pid.clone(), None, &mut scratch) {
                Ok(SemAttempt::Completed {
                    waiter_state_changed: changed,
                }) => {
                    set.finish_and_wake(queue, entry.clone(), Ok(0), wakes);
                    if changed {
                        return true;
                    }
                }
                Ok(SemAttempt::Blocked(blocker)) if blocker.nowait => {
                    set.finish_and_wake(
                        queue,
                        entry.clone(),
                        Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
                        wakes,
                    );
                }
                Ok(SemAttempt::Blocked(blocker)) => {
                    set.update_blocker(&entry, blocker);
                }
                Err(error) => {
                    set.finish_and_wake(queue, entry.clone(), Err(error), wakes);
                }
            }
        }
        false
    }

    /// Complete executable const entries before altering entries.
    pub(in crate::ipc::sem) fn update_queue(&mut self, wakes: &mut SemWakeBatch) {
        let set = self;
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
        match set.try_apply(&entry.sops, entry.pid.clone(), Some(record), &mut scratch) {
            Ok(SemAttempt::Completed {
                waiter_state_changed,
            }) => Ok(Some(waiter_state_changed)),
            Ok(SemAttempt::Blocked(blocker)) => {
                if blocker.nowait {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                } else {
                    set.update_blocker(entry, blocker);
                    Ok(None)
                }
            }
            Err(error) => Err(error),
        }
    }
}

impl KernelSemSet {
    /// Manager lock serializes unlink, counter removal and terminal publication.
    /// A losing cancellation/completion preserves the first result and never double-wakes.
    pub(in crate::ipc::sem) fn finish_waiter(
        &mut self,
        entry: &Arc<SemQueueEntry>,
        result: Result<usize, SystemError>,
    ) -> bool {
        self.finish_pending(Self::pending_queue_for(&entry.sops), entry, result)
    }

    fn finish_pending(
        &mut self,
        queue: SemPendingQueue,
        entry: &Arc<SemQueueEntry>,
        result: Result<usize, SystemError>,
    ) -> bool {
        self.remove_pending(queue, entry);
        entry.complete(result)
    }
    fn finish_and_wake(
        &mut self,
        queue: SemPendingQueue,
        entry: Arc<SemQueueEntry>,
        result: Result<usize, SystemError>,
        wakes: &mut SemWakeBatch,
    ) {
        if self.finish_pending(queue, &entry, result) {
            wakes.push_completed(entry);
        }
    }
}

#[cfg(test)]
mod tests;
