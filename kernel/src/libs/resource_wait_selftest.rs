//! Isolated resource mutex contention, using the real registration and guards.
use super::*;
use crate::{
    mm::oom::denial,
    process::kthread::{KernelThreadClosure, KernelThreadMechanism},
    smp::{core::smp_get_processor_id, cpu::smp_cpu_manager},
    time::{clocksource::HZ, timer::clock},
};
use alloc::{boxed::Box, string::ToString, vec::Vec};
use core::sync::atomic::AtomicBool;

fn bounded_poll(mut ready: impl FnMut() -> bool) -> bool {
    let began = clock();
    for _ in 0..20_000_000 {
        if ready() {
            return true;
        }
        if clock().wrapping_sub(began) >= 5 * HZ {
            break;
        }
        core::hint::spin_loop();
    }
    false
}

pub(crate) fn mutex_protocol() -> Result<bool, SystemError> {
    let Some(worker_cpu) = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .find(|cpu| *cpu != smp_get_processor_id() && smp_cpu_manager().is_online_cpu(*cpu))
    else {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    };
    let lock = Arc::new(ResourceMutex::new(0usize));
    for round in 0..3 {
        let done = Arc::new(AtomicUsize::new(0));
        let passed = Arc::new(AtomicBool::new(true));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let worker_lock = lock.clone();
            let worker_done = done.clone();
            let worker_passed = passed.clone();
            let closure = KernelThreadClosure::EmptyClosure((
                Box::new(move || {
                    let original = denial::is_current_denied();
                    let ok = {
                        let _deny = denial::Scope::enter(true);
                        let before = denial::probe_denied();
                        let nonpreemptible = if round == 2 {
                            let task = ProcessManager::current_pcb();
                            let count = task.preempt_count();
                            let mut held = NonPreemptibleResourceGuard::lock(&worker_lock);
                            let suppressed = task.preempt_count() == count + 1;
                            *held += 1;
                            drop(held);
                            suppressed && task.preempt_count() == count
                        } else {
                            let mut held = ResourceMutexGuard::lock(&worker_lock);
                            *held += 1;
                            drop(held);
                            true
                        };
                        before && nonpreemptible && denial::probe_denied()
                    };
                    if !ok || denial::is_current_denied() != original {
                        worker_passed.store(false, Ordering::Release);
                    }
                    worker_done.fetch_add(1, Ordering::Release);
                    0
                }),
                (),
            ));
            let Some(worker) = KernelThreadMechanism::create_on_cpu(
                closure,
                "resource-mutex-worker".to_string(),
                worker_cpu,
            ) else {
                for worker in workers {
                    let _ = KernelThreadMechanism::stop(&worker);
                }
                return Err(SystemError::ENOMEM);
            };
            workers.push(worker);
        }
        // A try-only owner must participate in exactly the same unlock/wake
        // protocol as a blocking acquisition. Both contenders really register.
        let held = ResourceMutexGuard::try_lock(&lock).ok_or(SystemError::EBUSY)?;
        let key = held.resource_key_for_test();
        let mut registered = ResourceMutexGuard::try_lock(&lock).is_none()
            && lock.wait_queue.waiters.load(Ordering::Acquire) == 0;
        for worker in &workers {
            registered &= ProcessManager::wakeup(worker).is_ok()
                && bounded_poll(|| is_waiting_for_resource_for_test(worker, key));
        }
        registered &= lock.wait_queue.waiters.load(Ordering::Acquire) == 2;
        drop(held);
        let completed = registered && bounded_poll(|| done.load(Ordering::Acquire) == 2);
        if !completed {
            // Rescue is only cleanup after recording a failed observation.
            // It never changes the result to success.
            for worker in &workers {
                let _ = ProcessManager::wakeup(worker);
            }
        }
        if !bounded_poll(|| done.load(Ordering::Acquire) == 2) {
            for worker in &workers {
                let _ = KernelThreadMechanism::request_stop(worker);
            }
            return Ok(false);
        }
        let mut joined = true;
        for worker in workers {
            joined &= matches!(KernelThreadMechanism::stop(&worker), Ok(0));
        }
        if !completed
            || !joined
            || !passed.load(Ordering::Acquire)
            || lock.wait_queue.waiters.load(Ordering::Acquire) != 0
            || *ResourceMutexGuard::lock(&lock) != (round + 1) * 2
        {
            return Ok(false);
        }
    }
    // Reuse after all registrations retire, including the killable success
    // path; fatal admission/error cleanup is covered by the shared wait RAII.
    let mut held = ResourceMutexGuard::lock_killable(&lock)?;
    *held += 1;
    drop(held);
    Ok(lock.wait_queue.waiters.load(Ordering::Acquire) == 0
        && ResourceMutexGuard::try_lock(&lock).is_some_and(|held| *held == 7))
}

// Private Queue fixtures share the production register/selection/migration and
// last-notification algorithms, but never dispatch scheduler wakeups.
fn fixture_register(queue: &SpinLock<Queue>, task: &Arc<ProcessControlBlock>, key: usize) {
    let retired = queue.lock_irqsave().register(task, key);
    drop(retired);
}

fn fixture_select(queue: &SpinLock<Queue>, key: usize) -> Option<Arc<ProcessControlBlock>> {
    let mut q = queue.lock_irqsave();
    let pending = q.notifications(key, false);
    if let Some(task) = pending.as_ref() {
        let mut link = task.resource_wait_link.lock();
        debug_assert!(link.notify_next.is_none());
        // Same pop boundary as dispatch_notifications, without scheduler wake.
        link.notify_queued = false;
    }
    drop(q);
    pending
}

fn fixture_cancel(queue: &SpinLock<Queue>, task: &Arc<ProcessControlBlock>) {
    let _q = queue.lock_irqsave();
    // Model only the cancellation publication while a notification is pinned;
    // this does not claim to test the global Registration Drop's relay itself.
    task.resource_wait_link.lock().registered = false;
}

fn fixture_finish(queue: &SpinLock<Queue>, task: &Arc<ProcessControlBlock>) -> bool {
    let (retired, needs_worker) = queue.lock_irqsave().finish_notification(task);
    drop(retired);
    needs_worker
}

fn fixture_clear(
    queue: &SpinLock<Queue>,
    a: &Arc<ProcessControlBlock>,
    b: &Arc<ProcessControlBlock>,
) {
    let mut q = queue.lock_irqsave();
    for task in [a, b] {
        let mut link = task.resource_wait_link.lock();
        debug_assert!(!link.notify_queued && link.notify_next.is_none());
        link.notifying = 0;
    }
    let ra = q.unlink(a);
    let rb = q.unlink(b);
    drop(q);
    drop(ra);
    drop(rb);
}

pub(crate) fn split_queue_protocol() -> Result<bool, SystemError> {
    let make_worker = || {
        KernelThreadMechanism::create(
            KernelThreadClosure::EmptyClosure((Box::new(|| 0), ())),
            "resource-split-fixture".to_string(),
        )
    };
    let a = make_worker().ok_or(SystemError::ENOMEM)?;
    let Some(b) = make_worker() else {
        return match KernelThreadMechanism::stop(&a) {
            Ok(_) => Err(SystemError::ENOMEM),
            Err(error) => Err(error),
        };
    };
    let mut private_queue = Queue::new();
    private_queue.isolated = true;
    let queue = SpinLock::new(private_queue);
    let empty = |task: &Arc<ProcessControlBlock>| {
        let link = task.resource_wait_link.lock();
        link.class.is_none()
            && !link.registered
            && link.notifying == 0
            && !link.notify_queued
            && link.next.is_none()
            && link.notify_next.is_none()
    };
    let prepared = empty(&a) && empty(&b);
    let originally_denied = denial::is_current_denied();
    let mut passed = prepared;
    if prepared {
        let _deny = denial::Scope::enter(true);
        passed &= denial::probe_denied();
        // The original maintenance counterexample must select A, not B.
        fixture_register(&queue, &a, 1);
        fixture_register(&queue, &b, 1);
        // Same live Registration retry must not reappend A behind B.
        fixture_register(&queue, &a, 1);
        let popped = queue.lock_irqsave().process_front();
        passed &= popped.is_none();
        drop(popped);
        let selected = fixture_select(&queue, 1);
        passed &= selected.as_ref().is_some_and(|task| Arc::ptr_eq(task, &a));
        if let Some(task) = selected.as_ref() {
            passed &= !fixture_finish(&queue, task);
        }
        drop(selected);
        fixture_clear(&queue, &a, &b);

        // A cancelled old registration must not jump ahead of B in a new key.
        fixture_register(&queue, &a, 1);
        let old = fixture_select(&queue, 1);
        fixture_cancel(&queue, &a);
        fixture_register(&queue, &b, 2);
        fixture_register(&queue, &a, 2);
        let new = fixture_select(&queue, 2);
        passed &= new.as_ref().is_some_and(|task| Arc::ptr_eq(task, &b));
        if let Some(task) = old.as_ref() {
            passed &= !fixture_finish(&queue, task);
        }
        if let Some(task) = new.as_ref() {
            passed &= !fixture_finish(&queue, task);
        }
        drop(old);
        drop(new);
        passed &= a.resource_wait_link.lock().registered;
        fixture_clear(&queue, &a, &b);

        // An old notification survives both class migrations without unlinking
        // the new registration. Test the real physical class and link counts.
        fixture_register(&queue, &a, 1);
        let old = fixture_select(&queue, 1);
        fixture_cancel(&queue, &a);
        fixture_register(&queue, &a, 0);
        {
            let q = queue.lock_irqsave();
            passed &= q.oom.len == 1
                && q.resource.len == 0
                && a.resource_wait_link.lock().class == Some(QueueClass::Oom);
        }
        fixture_cancel(&queue, &a);
        fixture_register(&queue, &a, 2);
        if let Some(task) = old.as_ref() {
            passed &= !fixture_finish(&queue, task);
        }
        drop(old);
        {
            let q = queue.lock_irqsave();
            passed &= q.oom.len == 0
                && q.resource.len == 1
                && a.resource_wait_link.lock().class == Some(QueueClass::Resource);
        }
        fixture_clear(&queue, &a, &b);

        // Finishing an old notification while the new OOM wait is active
        // requests progress, without retiring that new registration.
        fixture_register(&queue, &a, 1);
        let old = fixture_select(&queue, 1);
        fixture_cancel(&queue, &a);
        fixture_register(&queue, &a, 0);
        if let Some(task) = old.as_ref() {
            passed &= fixture_finish(&queue, task);
        } else {
            passed = false;
        }
        drop(old);
        passed &= a.resource_wait_link.lock().registered;
        let progressed = queue.lock_irqsave().process_front();
        passed &= progressed
            .as_ref()
            .is_some_and(|(task, _, wake)| Arc::ptr_eq(task, &a) && *wake);
        drop(progressed);
        fixture_clear(&queue, &a, &b);

        // Two in-flight notifications: only the last may retire cancellation.
        fixture_register(&queue, &a, 1);
        let one = fixture_select(&queue, 1);
        let two = fixture_select(&queue, 1);
        passed &= one.is_some() && two.is_some() && a.resource_wait_link.lock().notifying == 2;
        fixture_cancel(&queue, &a);
        if let Some(task) = one.as_ref() {
            passed &= !fixture_finish(&queue, task);
        }
        passed &= a.resource_wait_link.lock().class == Some(QueueClass::Resource);
        if let Some(task) = two.as_ref() {
            // Last cancelled pin preserves the original OOM progress kick.
            passed &= fixture_finish(&queue, task);
        }
        drop(one);
        drop(two);
        passed &= empty(&a) && queue.lock_irqsave().resource.len == 0;
        fixture_clear(&queue, &a, &b);

        // OOM progress remains independent and FIFO for unpinned entries.
        fixture_register(&queue, &a, 0);
        fixture_register(&queue, &b, 0);
        let first = queue.lock_irqsave().process_front();
        let second = queue.lock_irqsave().process_front();
        passed &= first
            .as_ref()
            .is_some_and(|(task, _, wake)| Arc::ptr_eq(task, &a) && *wake)
            && second
                .as_ref()
                .is_some_and(|(task, _, wake)| Arc::ptr_eq(task, &b) && *wake);
        drop(first);
        drop(second);
        fixture_clear(&queue, &a, &b);
        passed &= denial::probe_denied();
    }
    let clean = empty(&a) && empty(&b);
    let stopped_a = if clean {
        KernelThreadMechanism::stop(&a).is_ok()
    } else {
        false
    };
    let stopped_b = if clean {
        KernelThreadMechanism::stop(&b).is_ok()
    } else {
        false
    };
    Ok(passed
        && clean
        && stopped_a
        && stopped_b
        && denial::is_current_denied() == originally_denied)
}
