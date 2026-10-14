//! Deferred memory-progress notifications, never reclaim or OOM policy.
//!
//! A release can occur under an arbitrary resource lock. Its interrupt-side
//! notification must not upgrade and retire a user task's last PCB reference.
//! Only the permanently pinned bootstrap worker is woken in softirq context;
//! ordinary recovery waiters are broadcast from its resource-lock-free loop.

use alloc::{string::ToString, sync::Arc};
use core::sync::atomic::{fence, AtomicBool, Ordering};
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    exception::softirq::{softirq_vectors, SoftirqNumber, SoftirqVec},
    init::initcall::INITCALL_CORE,
    libs::{
        spinlock::SpinLock,
        wait_queue::{Waiter, Waker},
    },
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock, ProcessManager,
    },
    sched::completion::Completion,
};

use super::oom_wait::OomWaitQueue;

pub(crate) mod weak_registry;

pub(crate) static RECOVERY_WAITERS: OomWaitQueue = OomWaitQueue::new();

static READY: AtomicBool = AtomicBool::new(false);
static WORK_PENDING: AtomicBool = AtomicBool::new(false);
static HIGH_PENDING: AtomicBool = AtomicBool::new(false);
static FILE_EVENTS_PENDING: AtomicBool = AtomicBool::new(false);
static STARTED: Completion = Completion::new();
// These two pins are never removed after successful bootstrap. In particular,
// a softirq's temporary upgrade/clone cannot be the last PCB/Waker reference.
static WORKER: SpinLock<Option<Arc<ProcessControlBlock>>> = SpinLock::new(None);
static WORKER_WAKER: SpinLock<Option<Arc<Waker>>> = SpinLock::new(None);

pub(crate) fn is_ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Call after publishing actual admission/MM progress. No allocation, task
/// lookup, resource locking or direct scheduler callback occurs here.
pub(crate) fn request_oom_progress() {
    if !is_ready() {
        return;
    }
    // Paired with register's full barrier before its caller checks the actual
    // predicate. Like Linux waitqueue_active(), a lockless empty check must
    // not pass a preceding progress store (Release/Acquire alone is not enough).
    fence(Ordering::SeqCst);
    if !RECOVERY_WAITERS.has_registrations()
        && !crate::filesystem::cgroup2::events::has_memory_watchers()
    {
        return;
    }
    WORK_PENDING.store(true, Ordering::Release);
    softirq_vectors().raise_softirq(SoftirqNumber::MEMORY_NOTIFY);
}

/// Deadline IRQ callbacks only wake the pinned infrastructure worker. User
/// task ownership and the high waiter registry are touched in task context.
pub(crate) fn request_high_deadline() {
    if !is_ready() {
        return;
    }
    HIGH_PENDING.store(true, Ordering::Release);
    WORK_PENDING.store(true, Ordering::Release);
    softirq_vectors().raise_softirq(SoftirqNumber::MEMORY_NOTIFY);
}

/// Real file-source changes only. Ordinary page release must not scan inode
/// subscriptions when there is no new file event to deliver.
pub(crate) fn request_file_events() {
    if !is_ready() {
        return;
    }
    fence(Ordering::SeqCst);
    if !crate::filesystem::cgroup2::event_notify::has_watchers()
        && !crate::filesystem::cgroup2::events::has_watchers()
    {
        return;
    }
    FILE_EVENTS_PENDING.store(true, Ordering::Release);
    WORK_PENDING.store(true, Ordering::Release);
    softirq_vectors().raise_softirq(SoftirqNumber::MEMORY_NOTIFY);
}

#[derive(Debug)]
struct MemoryNotifySoftirq;

impl SoftirqVec for MemoryNotifySoftirq {
    fn run(&self) {
        if !is_ready() || !WORK_PENDING.load(Ordering::Acquire) {
            return;
        }
        let waker = WORKER_WAKER
            .lock_irqsave()
            .as_ref()
            .expect("ready memory notifier has no waker")
            .clone();
        // The temporary slot guard is gone before entering the scheduler.
        waker.wake();
    }
}

#[unified_init(INITCALL_CORE)]
fn init_memory_notifier() -> Result<(), SystemError> {
    // unified-init logs initializer errors and continues; this executor is
    // mandatory bootstrap infrastructure, so failure must be explicitly fatal.
    start_memory_notifier().expect("memory notifier bootstrap failed");
    Ok(())
}

fn start_memory_notifier() -> Result<(), SystemError> {
    let handler = Arc::try_new(MemoryNotifySoftirq).map_err(|_| SystemError::ENOMEM)?;
    let closure =
        KernelThreadClosure::StaticEmptyClosure((&(notification_loop as fn() -> i32), ()));
    // The existing per-CPU kthread contract also forbids cgroup migration.
    // Otherwise a privileged writer could place this progress executor into
    // the very constrained domain whose waiters it must wake.
    let worker = KernelThreadMechanism::create_on_cpu(
        closure,
        "memory_notify".to_string(),
        crate::smp::core::smp_get_processor_id(),
    )
    .ok_or(SystemError::ENOMEM)?;
    *WORKER.lock_irqsave() = Some(worker.clone());
    softirq_vectors().register_softirq(SoftirqNumber::MEMORY_NOTIFY, handler)?;
    ProcessManager::wakeup(&worker)?;
    STARTED.wait_for_completion()?;
    assert!(
        is_ready(),
        "memory notifier bootstrap did not publish readiness"
    );
    Ok(())
}

fn notification_loop() -> i32 {
    let (waiter, waker) =
        Waiter::try_new_pair().expect("memory notifier bootstrap waiter allocation failed");
    *WORKER_WAKER.lock_irqsave() = Some(waker);
    READY.store(true, Ordering::Release);
    STARTED.complete();
    loop {
        if WORK_PENDING.swap(false, Ordering::AcqRel) {
            if HIGH_PENDING.swap(false, Ordering::AcqRel) {
                crate::cgroup::memory::high::notify_deadlines();
            }
            RECOVERY_WAITERS.wake_all();
            crate::filesystem::cgroup2::events::notify();
            if FILE_EVENTS_PENDING.swap(false, Ordering::AcqRel) {
                crate::filesystem::cgroup2::event_notify::notify();
            }
        }
        // Producers store pending before notifying this permanent waiter.
        // The existing consume/prepare/block/recheck handshake closes both
        // the check-to-sleep race and notifications received while processing.
        if !WORK_PENDING.load(Ordering::Acquire) {
            waiter
                .wait(false)
                .expect("permanent memory notifier wait failed");
        }
    }
}
