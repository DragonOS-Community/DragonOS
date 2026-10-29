//! One permanently pinned, fixed-CPU executor for committed victim MMs.
//! Queue publication happens in the OOM commit transaction; scheduler wake and
//! all job/MM destruction happen after that transaction's locks are gone.

use alloc::{string::ToString, sync::Arc};
use core::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
};
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    init::initcall::INITCALL_POSTCORE,
    libs::{
        spinlock::SpinLock,
        wait_queue::{Waiter, Waker},
    },
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock, ProcessManager,
    },
    sched::completion::Completion,
    time::{
        deadline::{DeadlineCallback, DeadlineEvent},
        timekeeping::monotonic_now,
    },
};

use super::{OomReapQueue, ReapStep};
use crate::mm::ucontext::{oom_reap::OomReapAuthorization, AddressSpace};

static QUEUE: OomReapQueue = OomReapQueue::new();
static READY: AtomicBool = AtomicBool::new(false);
static STARTED: Completion = Completion::new();
static WORKER: SpinLock<Option<Arc<ProcessControlBlock>>> = SpinLock::new(None);
static WAKER: SpinLock<Option<Arc<Waker>>> = SpinLock::new(None);

/// Caller already committed SIGKILL and resolved shared-init protection in
/// the PID membership transaction. No callback, allocation or wake here.
pub(in crate::mm) fn queue_committed_mm(
    mm: &Arc<AddressSpace>,
    authorization: OomReapAuthorization,
) {
    if authorization.mm_id() != mm.id() {
        return;
    }
    if !cfg!(any(target_arch = "x86_64", target_arch = "riscv64")) {
        // This port has no usable user-MM/clockevent backend yet. Do not queue
        // an unserviceable job or panic on its first unsupported deadline.
        mm.oom_reap_slot()
            .finish(super::ReapFinish::UnsupportedMapping);
        return;
    }
    if let Some(job) =
        mm.oom_reap_slot()
            .claim_after_commit(mm, authorization, monotonic_now().to_ktime_ns())
    {
        QUEUE.push(job);
    }
}

pub(in crate::mm) fn wake_worker() {
    if READY.load(Ordering::Acquire) {
        let waker = WAKER
            .lock_irqsave()
            .as_ref()
            .expect("ready OOM reaper has a waker")
            .clone();
        waker.wake();
    }
}

struct ReaperDeadline(Arc<Waker>);

impl fmt::Debug for ReaperDeadline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReaperDeadline")
    }
}

impl DeadlineCallback for ReaperDeadline {
    fn expire(&self, _: u64) -> Option<u64> {
        self.0.wake();
        None
    }
}

#[unified_init(INITCALL_POSTCORE)]
#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
fn init_oom_reaper() -> Result<(), SystemError> {
    let closure = KernelThreadClosure::StaticEmptyClosure((&(reaper_loop as fn() -> i32), ()));
    let worker = KernelThreadMechanism::create_on_cpu(
        closure,
        "oom_reaper".to_string(),
        crate::smp::core::smp_get_processor_id(),
    )
    .expect("OOM reaper bootstrap birth failed");
    *WORKER.lock_irqsave() = Some(worker.clone());
    ProcessManager::wakeup(&worker).expect("OOM reaper first wake failed");
    STARTED
        .wait_for_completion()
        .expect("OOM reaper bootstrap wait failed");
    Ok(())
}

fn wait_deadline(waiter: &Waiter, event: &Arc<DeadlineEvent>, deadline: u64) {
    // An arrival/other wake is not the deadline. Always recheck monotonic time.
    while monotonic_now().to_ktime_ns() < deadline {
        event
            .arm(deadline)
            .expect("fixed-CPU OOM reaper deadline unavailable");
        waiter.wait(false).expect("OOM reaper deadline wait failed");
    }
    event.cancel();
}

fn reaper_loop() -> i32 {
    let (waiter, waker) = Waiter::try_new_pair().expect("OOM reaper waiter birth failed");
    let callback =
        Arc::try_new(ReaperDeadline(waker.clone())).expect("OOM reaper callback birth failed");
    let event = DeadlineEvent::try_new(crate::smp::core::smp_get_processor_id(), callback)
        .expect("OOM reaper deadline birth failed");
    *WAKER.lock_irqsave() = Some(waker);
    READY.store(true, Ordering::Release);
    STARTED.complete();
    loop {
        // The finite detached batch is not overtaken by new arrivals. Linux's
        // LIFO order is retained within the batch without an extra index.
        let mut batch = QUEUE.take_batch();
        while let Some(mut job) = batch {
            batch = job.next.take();
            let mm = job.mm.as_ref().expect("queued OOM job has MM").clone();
            let authorization = job.take_authorization();
            loop {
                match job.run_step(
                    mm.oom_reap_slot(),
                    &authorization,
                    monotonic_now().to_ktime_ns(),
                ) {
                    ReapStep::Continue => crate::sched::cond_resched(),
                    ReapStep::Delay { deadline_ns } => wait_deadline(&waiter, &event, deadline_ns),
                    ReapStep::Terminal(_) => break,
                }
            }
            // Finished means selection can move on; it does not report frame
            // release. Real mmput/admission release has its independent epoch.
            crate::mm::oom::note_oom_reaping_finished(mm.id());
            drop(job);
            drop(mm);
        }
        // Waiter consumes/prepare/block/recheck closes publication-to-sleep.
        // If a producer published before READY, the queue predicate finds it.
        if QUEUE.is_empty() {
            waiter.wait(false).expect("OOM reaper work wait failed");
        }
    }
}
