//! Allocation-free waits for OOM coordination and the lifecycle resource locks.
//!
//! One task can wait here once at a time. Queue ownership pins each PCB; all
//! resource wakes retain the queue pin; final-reference retirement happens
//! outside the queue lock.

use alloc::sync::Arc;
use core::sync::atomic::{fence, AtomicUsize, Ordering};
use system_error::SystemError;

use crate::{
    arch::ipc::signal::Signal,
    libs::spinlock::SpinLock,
    process::{ProcessControlBlock, ProcessFlags, ProcessManager},
    sched::{schedule, SchedMode},
};

/// Only the resource wait queue may access this link, in queue -> link order.
#[derive(Debug, Default)]
pub(crate) struct ResourceWaitLink {
    next: Option<Arc<ProcessControlBlock>>,
    registered: bool,
    linked: bool,
    notifying: usize,
    notify_next: Option<Arc<ProcessControlBlock>>,
    notify_queued: bool,
    resource_key: usize,
}

struct Queue {
    head: Option<Arc<ProcessControlBlock>>,
    tail: Option<Arc<ProcessControlBlock>>,
    len: usize,
}

static QUEUE: SpinLock<Queue> = SpinLock::new(Queue {
    head: None,
    tail: None,
    len: 0,
});
static REGISTERED: AtomicUsize = AtomicUsize::new(0);

impl Queue {
    fn register(&mut self, task: &Arc<ProcessControlBlock>, resource_key: usize) {
        let mut link = task.resource_wait_link.lock();
        if link.linked {
            debug_assert!(!link.registered || link.resource_key == resource_key);
            link.registered = true;
            link.resource_key = resource_key;
            return;
        }
        debug_assert!(link.next.is_none() && link.notifying == 0);
        link.registered = true;
        link.linked = true;
        link.resource_key = resource_key;
        if let Some(tail) = self.tail.as_ref() {
            tail.resource_wait_link.lock().next = Some(task.clone());
        } else {
            self.head = Some(task.clone());
        }
        // The previous tail remains pinned by the linked queue.
        self.tail = Some(task.clone());
        self.len += 1;
        REGISTERED.store(self.len, Ordering::Release);
    }

    /// Return removed owners for retirement after the caller unlocks the queue.
    fn unlink(
        &mut self,
        target: &Arc<ProcessControlBlock>,
    ) -> (
        Option<Arc<ProcessControlBlock>>,
        Option<Arc<ProcessControlBlock>>,
    ) {
        if !target.resource_wait_link.lock().linked {
            return (None, None);
        }
        let mut previous: Option<Arc<ProcessControlBlock>> = None;
        let mut cursor = self.head.clone();
        while let Some(task) = cursor {
            let mut link = task.resource_wait_link.lock();
            if Arc::ptr_eq(&task, target) {
                let removed = if let Some(previous) = previous.as_ref() {
                    let mut previous_link = previous.resource_wait_link.lock();
                    let removed = previous_link.next.take();
                    previous_link.next = link.next.take();
                    removed
                } else {
                    let removed = self.head.take();
                    self.head = link.next.take();
                    removed
                };
                debug_assert_eq!(link.notifying, 0);
                link.registered = false;
                link.linked = false;
                let old_tail = if self
                    .tail
                    .as_ref()
                    .is_some_and(|tail| Arc::ptr_eq(tail, &task))
                {
                    core::mem::replace(&mut self.tail, previous.clone())
                } else {
                    None
                };
                self.len -= 1;
                REGISTERED.store(self.len, Ordering::Release);
                // `removed` pins task; previous stays linked. Neither local
                // observer can be the final owner while the queue is locked.
                return (removed, old_tail);
            }
            cursor = link.next.clone();
            drop(link);
            previous = Some(task);
        }
        (None, None)
    }

    /// Snapshot matching waiters in one pass. Separate embedded notification
    /// links survive concurrent rotation/cancellation of the physical queue.
    fn notifications(&mut self, key: usize, all: bool) -> Option<Arc<ProcessControlBlock>> {
        debug_assert_ne!(key, 0);
        let mut pending = None;
        let mut cursor = self.head.clone();
        while let Some(task) = cursor {
            let mut link = task.resource_wait_link.lock();
            cursor = link.next.clone();
            if !link.registered || link.resource_key != key {
                continue;
            }
            if !link.notify_queued {
                debug_assert!(link.notify_next.is_none());
                link.notify_queued = true;
                link.notifying += 1;
                link.notify_next = pending.take();
                pending = Some(task.clone());
            }
            if !all {
                break;
            }
        }
        pending
    }

    /// One O(1) front operation per initial node. Busy notification pins and
    /// active resource waiters rotate; only the worker retires cancelled pins.
    fn process_front(
        &mut self,
    ) -> Option<(
        Arc<ProcessControlBlock>,
        Option<Arc<ProcessControlBlock>>,
        bool,
    )> {
        let task = self.head.as_ref()?.clone();
        let mut link = task.resource_wait_link.lock();
        if link.notifying != 0 || (link.registered && link.resource_key != 0) {
            if self.len > 1 {
                let old_head = self.head.take();
                self.head = link.next.take();
                self.tail.as_ref().unwrap().resource_wait_link.lock().next = old_head;
                // The old tail remains pinned through the new head's chain.
                self.tail = Some(task.clone());
            }
            return None;
        }
        let wake = link.registered;
        drop(link);
        let (removed, old_tail) = self.unlink(&task);
        Some((removed.expect("linked memory waiter"), old_tail, wake))
    }
}

struct Registration {
    task: Arc<ProcessControlBlock>,
    resource_key: usize,
    acquired: bool,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let (retired, notification) = {
            let mut queue = QUEUE.lock_irqsave();
            let mut link = self.task.resource_wait_link.lock();
            link.registered = false;
            let can_unlink = link.notifying == 0;
            drop(link);
            let retired = if can_unlink {
                queue.unlink(&self.task)
            } else {
                (None, None)
            };
            // If a woken first waiter cancels without acquiring the resource,
            // pass the release on even when no future mutex unlock is coming.
            let notification = if self.resource_key != 0 && !self.acquired {
                queue.notifications(self.resource_key, false)
            } else {
                None
            };
            (retired, notification)
        };
        drop(retired);
        dispatch_notifications(notification);
    }
}

/// Private immediate drain: callers cannot retain or discard a public batch.
/// Every selected owner is detached from the notification chain before ttwu.
fn dispatch_notifications(mut pending: Option<Arc<ProcessControlBlock>>) {
    while let Some(task) = pending.take() {
        {
            let _queue = QUEUE.lock_irqsave();
            let mut link = task.resource_wait_link.lock();
            debug_assert!(link.notify_queued && link.notifying != 0);
            pending = link.notify_next.take();
            // A second release after this pop must be allowed to queue a new
            // wake even if this ttwu is still in flight.
            link.notify_queued = false;
        }
        Notification { task: Some(task) }.wake();
    }
}

struct Notification {
    task: Option<Arc<ProcessControlBlock>>,
}

impl Notification {
    fn wake(self) {
        let _ = ProcessManager::wakeup(self.task.as_ref().unwrap());
        // Drop ends the pin after ttwu, including errors and early returns.
    }
}

impl Drop for Notification {
    fn drop(&mut self) {
        let task = self.task.take().unwrap();
        let needs_worker = {
            let _queue = QUEUE.lock_irqsave();
            let mut link = task.resource_wait_link.lock();
            debug_assert!(link.linked && link.notifying != 0);
            link.notifying -= 1;
            let needs_worker = link.notifying == 0 && (!link.registered || link.resource_key == 0);
            drop(link);
            // QUEUE excludes any unlink until this observer is gone. Its
            // physical chain owner prevents final PCB destruction here.
            drop(task);
            needs_worker
        };
        if needs_worker {
            crate::mm::memory_notify::request_oom_progress();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WaitMode {
    Uninterruptible,
    Interruptible,
    Killable,
}

impl WaitMode {
    fn interrupted(self, task: &Arc<ProcessControlBlock>) -> bool {
        match self {
            Self::Uninterruptible => false,
            Self::Interruptible => {
                Signal::signal_pending_state(true, false, task)
                    || task.flags().contains(ProcessFlags::EXITING)
            }
            Self::Killable => {
                Signal::oom_fatal_signal_pending(task)
                    || task.flags().contains(ProcessFlags::EXITING)
            }
        }
    }
}

pub(crate) fn wait_until_killable<R>(
    condition: impl FnMut() -> Option<R>,
) -> Result<R, SystemError> {
    wait(0, WaitMode::Killable, condition)
}

/// Opt-in lock slowpath. The resource borrow must outlive registration; the
/// predicate may notify other waiters, but must neither allocate nor sleep.
pub(crate) fn wait_for_resource<R>(
    key: usize,
    interruptible: bool,
    condition: impl FnMut() -> Option<R>,
) -> Result<R, SystemError> {
    debug_assert_ne!(key, 0);
    wait(
        key,
        if interruptible {
            WaitMode::Interruptible
        } else {
            WaitMode::Uninterruptible
        },
        condition,
    )
}

fn wait<R>(
    resource_key: usize,
    mode: WaitMode,
    mut condition: impl FnMut() -> Option<R>,
) -> Result<R, SystemError> {
    let task = ProcessManager::current_pcb();
    if mode.interrupted(&task) {
        return Err(SystemError::ERESTARTSYS);
    }
    if let Some(result) = condition() {
        return Ok(result);
    }
    let mut registration = Registration {
        task,
        resource_key,
        acquired: false,
    };
    loop {
        // Unlike Linux, this scheduler also parks non-runnable current tasks
        // on SM_PREEMPT. Prevent an IRQ from scheduling between prepare_sleep
        // and the outside-queue predicate which might already be satisfied.
        let preempt = crate::process::preempt::PreemptGuard::new();
        {
            let mut queue = QUEUE.lock_irqsave();
            queue.register(&registration.task, resource_key);
            fence(Ordering::SeqCst);
            if mode.interrupted(&registration.task) {
                drop(queue);
                return Err(SystemError::ERESTARTSYS);
            }
            if let Err(error) = ProcessManager::mark_sleep(mode != WaitMode::Uninterruptible) {
                drop(queue);
                return Err(error);
            }
        }
        // Ordinary RwSem rollback can notify recursively, so no queue lock
        // may cover this predicate. Sleeping state was published beforehand.
        if let Some(result) = condition() {
            ProcessManager::undo_mark_sleep();
            registration.acquired = true;
            drop(registration);
            drop(preempt);
            return Ok(result);
        }
        if mode.interrupted(&registration.task) {
            ProcessManager::undo_mark_sleep();
            drop(registration);
            drop(preempt);
            return Err(SystemError::ERESTARTSYS);
        }
        drop(preempt);
        schedule(SchedMode::SM_NONE);
    }
}

/// Broadcast every current waiter on this resource. The single queue-locked
/// snapshot cannot miss nodes due to concurrent broadcasters rotating them.
pub(crate) fn notify_resource_all(key: usize) {
    fence(Ordering::SeqCst);
    if has_waiters() {
        let pending = QUEUE.lock_irqsave().notifications(key, true);
        dispatch_notifications(pending);
    }
}

/// Narrow wrapper for lifecycle mutexes with an owner-stable address. All acquisitions,
/// including try-only ones, must return this guard so every unlock can notify.
#[must_use]
#[derive(Debug)]
pub struct ResourceMutexGuard<'a, T> {
    inner: Option<crate::libs::mutex::MutexGuard<'a, T>>,
    resource_key: usize,
}

impl<'a, T> ResourceMutexGuard<'a, T> {
    #[cfg(feature = "mm_selftest")]
    pub(crate) fn resource_key_for_test(&self) -> usize {
        self.resource_key
    }

    fn wrap(
        lock: &'a crate::libs::mutex::Mutex<T>,
        inner: crate::libs::mutex::MutexGuard<'a, T>,
    ) -> Self {
        Self {
            inner: Some(inner),
            resource_key: lock as *const _ as usize,
        }
    }

    pub(crate) fn try_lock(lock: &'a crate::libs::mutex::Mutex<T>) -> Option<Self> {
        lock.try_lock().ok().map(|inner| Self::wrap(lock, inner))
    }

    pub(crate) fn lock(lock: &'a crate::libs::mutex::Mutex<T>) -> Self {
        if let Some(guard) = Self::try_lock(lock) {
            return guard;
        }
        let key = lock as *const _ as usize;
        let inner = wait(key, WaitMode::Uninterruptible, || lock.try_lock().ok())
            .expect("exited task cannot wait for a memory resource");
        Self::wrap(lock, inner)
    }

    pub(crate) fn lock_killable(
        lock: &'a crate::libs::mutex::Mutex<T>,
    ) -> Result<Self, SystemError> {
        // Preserve fatal-signal admission even when the mutex is free.
        let inner = wait(lock as *const _ as usize, WaitMode::Killable, || {
            lock.try_lock().ok()
        })?;
        Ok(Self::wrap(lock, inner))
    }
}

impl<T> core::ops::Deref for ResourceMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.inner.as_ref().unwrap()
    }
}

impl<T> core::ops::DerefMut for ResourceMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.inner.as_mut().unwrap()
    }
}

impl<T> Drop for ResourceMutexGuard<'_, T> {
    fn drop(&mut self) {
        drop(self.inner.take());
        // Pair the unlock store with register-before-predicate's full barrier.
        // An Acquire empty check alone would permit both sides to miss progress.
        fence(Ordering::SeqCst);
        if has_waiters() {
            let pending = QUEUE.lock_irqsave().notifications(self.resource_key, false);
            dispatch_notifications(pending);
        }
    }
}

pub(crate) fn has_waiters() -> bool {
    REGISTERED.load(Ordering::Acquire) != 0
}

/// FIFO ordering makes the initial population a finite broadcast bound. New
/// registrations append behind it, so a reawakened task cannot hide old waiters.
/// Call only from the resource-lock-free memory notifier task.
pub(crate) fn wake_all() {
    let limit = QUEUE.lock_irqsave().len;
    for _ in 0..limit {
        let popped = QUEUE.lock_irqsave().process_front();
        let Some((task, old_tail, wake)) = popped else {
            continue;
        };
        if wake {
            let _ = ProcessManager::wakeup(&task);
        }
        drop(old_tail);
        drop(task);
    }
}

/// Observe only registration, never the resource payload or scheduler state.
#[cfg(feature = "mm_selftest")]
pub(crate) fn is_waiting_for_resource_for_test(
    task: &Arc<ProcessControlBlock>,
    key: usize,
) -> bool {
    let _queue = QUEUE.lock_irqsave();
    let link = task.resource_wait_link.lock();
    link.registered && link.resource_key == key
}
