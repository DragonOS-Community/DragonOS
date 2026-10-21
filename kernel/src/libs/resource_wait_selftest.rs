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
                        let mut held = ResourceMutexGuard::lock(&worker_lock);
                        *held += 1;
                        drop(held);
                        before && denial::probe_denied()
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
