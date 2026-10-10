//! Process CPU-clock progress notifications. Producers hold rq locks; wakeups
//! run in the existing tasklet bottom half, never inside those locks.
use alloc::{collections::BTreeMap, sync::Arc};
use core::sync::atomic::{fence, AtomicU64, Ordering};
use system_error::SystemError;

use crate::{
    exception::tasklet::{tasklet_schedule, Tasklet},
    libs::{spinlock::SpinLock, wait_queue::WaitQueue},
    process::ProcessState,
};

pub(super) struct CpuTimeWait {
    queue: WaitQueue,
    runtime: AtomicU64,
    deadlines: SpinLock<BTreeMap<u64, usize>>,
    earliest: AtomicU64,
    tasklet: Arc<Tasklet>,
}

impl CpuTimeWait {
    pub(super) fn new() -> Arc<Self> {
        Arc::new_cyclic(|weak: &alloc::sync::Weak<Self>| {
            let target = weak.clone();
            Self {
                queue: WaitQueue::default(),
                runtime: AtomicU64::new(0),
                deadlines: SpinLock::new(BTreeMap::new()),
                earliest: AtomicU64::new(u64::MAX),
                tasklet: Tasklet::new(
                    move |_, _| {
                        if let Some(target) = target.upgrade() {
                            // Tasklet clears its scheduled bit before invoking
                            // us. Pair with account() to preserve the last
                            // notification when concurrent producers coalesce.
                            fence(Ordering::SeqCst);
                            if target.due() {
                                target.queue.wakeup_all(Some(ProcessState::Blocked(true)));
                            }
                        }
                    },
                    0,
                    None,
                ),
            }
        })
    }

    fn due(&self) -> bool {
        self.runtime.load(Ordering::Acquire) >= self.earliest.load(Ordering::Acquire)
    }

    pub(super) fn runtime(&self) -> u64 {
        self.runtime.load(Ordering::Acquire)
    }

    pub(super) fn account(&self, delta: u64) {
        self.runtime.fetch_add(delta, Ordering::Release);
        // Pairs with wait()'s registration barrier. Do not substitute the
        // advisory queue.is_empty(): skipping the final wake could strand a
        // waiter whose worker exits immediately after reaching the deadline.
        fence(Ordering::SeqCst);
        if self.due() {
            tasklet_schedule(&self.tasklet);
        }
    }

    pub(super) fn wait<F: FnMut() -> bool>(
        &self,
        deadline: u64,
        condition: F,
    ) -> Result<(), SystemError> {
        struct Registered<'a>(&'a CpuTimeWait, u64);
        impl Drop for Registered<'_> {
            fn drop(&mut self) {
                let mut deadlines = self.0.deadlines.lock_irqsave();
                let count = deadlines.get_mut(&self.1).expect("registered CPU deadline");
                *count -= 1;
                if *count == 0 {
                    deadlines.remove(&self.1);
                }
                self.0.earliest.store(
                    deadlines.keys().next().copied().unwrap_or(u64::MAX),
                    Ordering::Release,
                );
            }
        }
        {
            let mut deadlines = self.deadlines.lock_irqsave();
            *deadlines.entry(deadline).or_default() += 1;
            self.earliest
                .store(*deadlines.keys().next().unwrap(), Ordering::Release);
        }
        let _registered = Registered(self, deadline);
        fence(Ordering::SeqCst);
        self.queue.wait_event_interruptible(condition, None::<fn()>)
    }
}

impl super::ProcessSignalState {
    pub(crate) fn account_cpu_runtime(&self, delta: u64) {
        self.cpu_time_wait.account(delta);
    }

    pub(crate) fn cpu_runtime(&self) -> u64 {
        self.cpu_time_wait.runtime()
    }

    pub(crate) fn wait_cpu_time<F: FnMut() -> bool>(
        &self,
        deadline: u64,
        condition: F,
    ) -> Result<(), SystemError> {
        self.cpu_time_wait.wait(deadline, condition)
    }
}
