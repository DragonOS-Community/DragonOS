//! OOM control storage prepared by normal task publication, never by OOM.
//!
//! One lease belongs to the globally serialized selector. Publishers can grow
//! a spare while that lease is in use; neither side waits for the other's work.
use alloc::{sync::Arc, vec::Vec};
use core::ops::{Deref, DerefMut};
use system_error::SystemError;

use crate::{
    libs::spinlock::SpinLock,
    mm::ucontext::AddressSpace,
    process::{ProcessControlBlock, ProcessManager, RawPid},
};

#[derive(Debug)]
pub(super) struct TaskRecord {
    pub task: Arc<ProcessControlBlock>,
    pub tgid: RawPid,
    pub dispatch: bool,
    /// Pin observed MMs until all IRQ guards are gone, even across exec.
    pub observed_mm: Option<Arc<AddressSpace>>,
    pub observed_oom_mm: Option<Arc<AddressSpace>>,
    /// Only a primary/group commit records an MM and an event here.
    pub killed_mm: Option<Arc<AddressSpace>>,
    pub retired_mm: Option<Arc<AddressSpace>>,
}

impl TaskRecord {
    pub(super) fn new(task: &Arc<ProcessControlBlock>) -> Self {
        Self {
            task: task.clone(),
            tgid: task.raw_tgid(),
            dispatch: false,
            observed_mm: None,
            observed_oom_mm: None,
            killed_mm: None,
            retired_mm: None,
        }
    }
}

pub(super) struct KilledMm {
    pub mm: Arc<AddressSpace>,
    pub shared_init: bool,
}

pub(super) struct Workspace {
    pub tasks: Vec<TaskRecord>,
    pub mms: Vec<KilledMm>,
}

impl Workspace {
    const fn new() -> Self {
        Self {
            tasks: Vec::new(),
            mms: Vec::new(),
        }
    }

    fn capacity(&self) -> usize {
        self.tasks.capacity().min(self.mms.capacity())
    }

    fn prepare(required: usize) -> Result<Self, SystemError> {
        let capacity = required
            .checked_next_power_of_two()
            .ok_or(SystemError::ENOMEM)?;
        let mut storage = Self::new();
        storage
            .tasks
            .try_reserve_exact(capacity)
            .map_err(|_| SystemError::ENOMEM)?;
        storage
            .mms
            .try_reserve_exact(capacity)
            .map_err(|_| SystemError::ENOMEM)?;
        Ok(storage)
    }
}

struct Pool {
    spare: Workspace,
    checked_out: Option<usize>,
}

impl Pool {
    fn capacity(&self) -> usize {
        self.spare.capacity().max(self.checked_out.unwrap_or(0))
    }
}

static POOL: SpinLock<Pool> = SpinLock::new(Pool {
    spare: Workspace::new(),
    checked_out: None,
});

/// Check again under final PID publication, before any irreversible attach.
pub(crate) fn has_capacity(required: usize) -> bool {
    POOL.lock_irqsave().capacity() >= required
}

/// All allocation and discarded backing destruction happen outside IRQ locks.
pub(crate) fn reserve(required: usize) -> Result<(), SystemError> {
    if has_capacity(required) {
        return Ok(());
    }
    let mut prepared = Workspace::prepare(required)?;
    {
        let mut pool = POOL.lock_irqsave();
        if pool.capacity() < required {
            core::mem::swap(&mut pool.spare, &mut prepared);
        }
    }
    drop(prepared);
    Ok(())
}

pub(super) struct Lease(Workspace);

impl Lease {
    /// Caller already owns OOM_STATE.selecting, including scoped OOM.
    pub(super) fn checkout() -> Self {
        let mut pool = POOL.lock_irqsave();
        assert!(
            pool.checked_out.is_none(),
            "OOM selector owns one workspace lease"
        );
        let storage = core::mem::replace(&mut pool.spare, Workspace::new());
        pool.checked_out = Some(storage.capacity());
        Self(storage)
    }

    /// Caller holds PID membership. Publication prepared either this lease or
    /// the spare for every task, even if a fork grew the registry after checkout.
    pub(super) fn fill(&mut self) {
        loop {
            let capacity = self.0.capacity();
            match ProcessManager::fill_snapshot_records(
                &mut self.0.tasks,
                capacity,
                TaskRecord::new,
            ) {
                Ok(()) => {
                    for record in &mut self.0.tasks {
                        record.observed_mm = record
                            .task
                            .with_task_lock_irqsave(|| record.task.basic().user_vm());
                    }
                    self.0
                        .tasks
                        .sort_unstable_by_key(|record| record.tgid.data());
                    return;
                }
                Err(required) => {
                    let mut pool = POOL.lock_irqsave();
                    assert!(
                        pool.spare.capacity() >= required,
                        "task publication prepared OOM control capacity"
                    );
                    core::mem::swap(&mut pool.spare, &mut self.0);
                    pool.checked_out = Some(self.0.capacity());
                }
            }
        }
    }
}

impl Deref for Lease {
    type Target = Workspace;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for Lease {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        // May release the last task/MM owner: no publication/IRQ guard here.
        self.0.tasks.clear();
        self.0.mms.clear();
        {
            let mut pool = POOL.lock_irqsave();
            if self.0.capacity() > pool.spare.capacity() {
                core::mem::swap(&mut self.0, &mut pool.spare);
            }
            pool.checked_out = None;
        }
        // Remaining empty backing is destroyed outside the pool lock.
    }
}
