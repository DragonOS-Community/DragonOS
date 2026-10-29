//! Repeated real worker sleeps and fallible work birth under heap denial.
use super::*;
use crate::{
    mm::oom::denial,
    process::{ProcessFlags, ProcessManager},
    time::{clocksource::HZ, timer::clock},
};
use core::sync::atomic::AtomicUsize;

fn poll(mut condition: impl FnMut() -> bool) -> bool {
    let began = clock();
    for step in 0..20_000_000 {
        if condition() {
            return true;
        }
        if clock().wrapping_sub(began) >= 5 * HZ {
            break;
        }
        if step % 256 == 0 {
            crate::sched::cond_resched();
        }
        core::hint::spin_loop();
    }
    false
}

struct WorkerDenial {
    task: Arc<ProcessControlBlock>,
    previously_denied: bool,
}

impl Drop for WorkerDenial {
    fn drop(&mut self) {
        self.task
            .flags()
            .finish_oom_heap_scope(self.previously_denied);
    }
}

pub(super) fn run() -> Result<bool, SystemError> {
    // Every allocation, including both callbacks, precedes either denial scope.
    let task_slot = Arc::new(SpinLock::new(None));
    let completed = Arc::new(AtomicUsize::new(0));
    let probes_ok = Arc::new(AtomicBool::new(true));
    let worker_slot = task_slot.clone();
    let identify = Work::new(move || {
        *worker_slot.lock() = Some(ProcessManager::current_pcb());
    });
    let worker_completed = completed.clone();
    let worker_probes = probes_ok.clone();
    let probe = Work::new(move || {
        if !denial::probe_denied() {
            worker_probes.store(false, Ordering::Release);
        }
        worker_completed.fetch_add(1, Ordering::Release);
    });
    let queue = WorkQueue::try_new_owned("wq-heap-test")?;
    let birth_rejected = {
        let _scope = denial::Scope::enter(true);
        let before = denial::probe_denied();
        let payload = [0u8; 64];
        let result = Work::try_new(move || {
            core::hint::black_box(payload);
        });
        before && matches!(result, Err(SystemError::ENOMEM)) && denial::probe_denied()
    };
    queue.enqueue(identify);
    if !poll(|| task_slot.lock().is_some()) {
        drop(queue);
        return Ok(false);
    }
    let task = task_slot.lock().as_ref().unwrap().clone();
    let key = Arc::as_ptr(&queue.wake_token) as usize;
    let asleep = || {
        resource_wait::is_waiting_for_resource_for_test(&task, key)
            && task.sched_info().state().is_blocked()
            && !task.sched_info().is_running()
    };
    let initially_asleep = poll(asleep);
    let previously_denied = task.flags().contains(ProcessFlags::OOM_HEAP_DENIED);
    let denial_guard = initially_asleep.then(|| {
        task.flags().insert(ProcessFlags::OOM_HEAP_DENIED);
        WorkerDenial {
            task: task.clone(),
            previously_denied,
        }
    });
    let mut repeated = initially_asleep;
    if initially_asleep {
        // Reuse one actual Work to check both idle re-entry and PENDING reset.
        for expected in 1..=2 {
            queue.enqueue(probe.clone());
            if !poll(|| completed.load(Ordering::Acquire) == expected && asleep()) {
                repeated = false;
                break;
            }
        }
    }
    drop(denial_guard);
    // No queue owner is captured by a callback or retained across worker sleep.
    // Observe natural last-owner exit before any rescue/stop can wake it.
    drop(queue);
    let exited_naturally = poll(|| task.sched_info().state().is_exited());
    let joined = if exited_naturally {
        matches!(KernelThreadMechanism::stop(&task), Ok(0))
    } else {
        let _ = KernelThreadMechanism::request_stop(&task);
        if poll(|| task.sched_info().state().is_exited()) {
            let _ = KernelThreadMechanism::stop(&task);
        }
        false
    };
    Ok(birth_rejected
        && repeated
        && completed.load(Ordering::Acquire) == 2
        && probes_ok.load(Ordering::Acquire)
        && exited_naturally
        && joined)
}
