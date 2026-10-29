//! Isolated checks of the production completion and kthread join protocol.
use super::Completion;
use crate::{
    libs::resource_wait,
    mm::oom::denial,
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessManager,
    },
    time::{clocksource::HZ, timer::clock},
};
use alloc::{boxed::Box, string::ToString, sync::Arc};
use core::sync::atomic::{AtomicBool, Ordering};
use system_error::SystemError;

fn poll_until(mut ready: impl FnMut() -> bool) -> bool {
    let end = clock().saturating_add(2 * HZ);
    loop {
        if ready() {
            return true;
        }
        if clock() >= end {
            return false;
        }
        crate::sched::sched_yield();
    }
}

pub(crate) fn wait_protocol_selftest() -> Result<bool, SystemError> {
    let mut count = Completion::new();
    let mut passed = release_transfers_notification(true)?
        && release_transfers_notification(false)?
        && !count.try_wait_for_completion();
    count.complete();
    count.complete();
    passed &= count.try_wait_for_completion()
        && count.try_wait_for_completion()
        && !count.try_wait_for_completion();
    count.inner.lock_irqsave().done = super::COMPLETE_ALL - 1;
    count.complete();
    passed &= count.inner.lock_irqsave().done == super::COMPLETE_ALL;
    passed &= count.try_wait_for_completion() && count.try_wait_for_completion();
    count.complete_all();
    passed &= count.try_wait_for_completion()
        && count.try_wait_for_completion()
        && count.completion_done();

    let mut modes = Completion::new();
    {
        let _denied = denial::Scope::enter(true);
        modes.complete();
        passed &= modes.wait_for_completion_interruptible() == Ok(i64::MAX);
        modes.complete();
        passed &= modes.wait_for_completion_killable() == Ok(i64::MAX);
        passed &= modes.wait_for_completion_interruptible_timeout(0) == Ok(0);
    }

    let timeout = Completion::new();
    {
        let _denied = denial::Scope::enter(true);
        passed &= timeout.wait_for_completion_timeout(0) == Ok(0);
        passed &= timeout.wait_for_completion_timeout(100) == Err(SystemError::ENOMEM);
        passed &= !timeout.completion_done();
        timeout.complete();
        passed &= timeout.wait_for_completion_timeout(0) == Ok(1);
        passed &= timeout.wait_for_completion_timeout(0) == Ok(0);
    }
    timeout.complete();
    passed &= timeout
        .wait_for_completion_timeout(100)
        .is_ok_and(|left| left > 0);
    let started = clock();
    passed &=
        timeout.wait_for_completion_timeout(2) == Ok(0) && clock().saturating_sub(started) >= 2;

    let completion = Arc::try_new(Completion::new()).map_err(|_| SystemError::ENOMEM)?;
    let child_completion = completion.clone();
    let returned = Arc::try_new(AtomicBool::new(false)).map_err(|_| SystemError::ENOMEM)?;
    let child_returned = returned.clone();
    let joined_wait = Arc::try_new(AtomicBool::new(false)).map_err(|_| SystemError::ENOMEM)?;
    let child_joined_wait = joined_wait.clone();
    let parent = ProcessManager::current_pcb();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let woke = {
                let _denied = denial::Scope::enter(true);
                child_completion.wait_for_completion() == Ok(i64::MAX)
            };
            child_returned.store(woke, Ordering::Release);
            let current = ProcessManager::current_pcb();
            let exited = current
                .worker_private()
                .as_ref()
                .and_then(|private| private.kernel_thread())
                .expect("completion fixture is a kthread")
                .exited_completion();
            // Do not exit until the stopper really registered on the exit
            // completion. This distinguishes the slow join from an exited fastpath.
            let registered = poll_until(|| {
                KernelThreadMechanism::should_stop(&current)
                    && resource_wait::is_waiting_for_resource_for_test(
                        &parent,
                        exited.wait_queue.key(),
                    )
            });
            child_joined_wait.store(registered, Ordering::Release);
            usize::from(!(woke && registered)) as i32
        }),
        (),
    ));
    let worker = KernelThreadMechanism::create(closure, "completion-protocol-test".to_string())
        .ok_or(SystemError::ENOMEM)?;
    let woke = ProcessManager::wakeup(&worker).is_ok();
    let registered = woke
        && poll_until(|| {
            resource_wait::is_waiting_for_resource_for_test(&worker, completion.wait_queue.key())
        });
    // Always release and join the worker, including a failed observation.
    completion.complete();
    let joined = {
        let _denied = denial::Scope::enter(true);
        KernelThreadMechanism::stop(&worker)
    };
    passed &= registered
        && returned.load(Ordering::Acquire)
        && joined_wait.load(Ordering::Acquire)
        && joined == Ok(0);
    Ok(passed)
}

/// Force a release between a predicate's terminal decision and registration
/// retirement. Both cancellation and consecutive counted releases must pass
/// the remaining ticket to a sleeper.
fn release_transfers_notification(cancel: bool) -> Result<bool, SystemError> {
    use crate::libs::resource_wait::WaitMode;
    use crate::smp::{core::smp_get_processor_id, cpu::smp_cpu_manager};
    let current_cpu = smp_get_processor_id();
    let remote_cpu = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .find(|cpu| *cpu != current_cpu && smp_cpu_manager().is_online_cpu(*cpu))
        .ok_or(SystemError::ENODEV)?;
    let completion = Arc::try_new(Completion::new()).map_err(|_| SystemError::ENOMEM)?;
    let decided = Arc::try_new(AtomicBool::new(false)).map_err(|_| SystemError::ENOMEM)?;
    let release = Arc::try_new(AtomicBool::new(false)).map_err(|_| SystemError::ENOMEM)?;
    let consumed = Arc::try_new(AtomicBool::new(false)).map_err(|_| SystemError::ENOMEM)?;

    let second_completion = completion.clone();
    let second_consumed = consumed.clone();
    let second = KernelThreadMechanism::create_on_cpu(
        KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                let success = second_completion.wait_for_completion() == Ok(i64::MAX);
                second_consumed.store(success, Ordering::Release);
                i32::from(!success)
            }),
            (),
        )),
        "completion-successor".to_string(),
        current_cpu,
    )
    .ok_or(SystemError::ENOMEM)?;
    let first_completion = completion.clone();
    let first_decided = decided.clone();
    let first_release = release.clone();
    let first = KernelThreadMechanism::create_on_cpu(
        KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                let mut initial = true;
                let result = first_completion.wait_queue.wait_with_outcome(
                    WaitMode::Uninterruptible,
                    || {
                        if initial {
                            initial = false;
                            return None;
                        }
                        // Reuse the actual completion decision, then retain that
                        // local result across another CPU's complete(). No test
                        // gate is added to the production completion predicate.
                        let outcome = first_completion.outcome(cancel.then(clock));
                        if outcome.is_some() {
                            first_decided.store(true, Ordering::Release);
                            let deadline = clock().saturating_add(2 * HZ);
                            while !first_release.load(Ordering::Acquire) && clock() < deadline {
                                core::hint::spin_loop();
                            }
                        }
                        outcome
                    },
                );
                let result = first_completion.finish_wait(result);
                i32::from(result != Ok(if cancel { 0 } else { i64::MAX }))
            }),
            (),
        )),
        "completion-cancel".to_string(),
        remote_cpu,
    );
    let Some(first) = first else {
        completion.complete_all();
        let _ = KernelThreadMechanism::stop(&second);
        return Err(SystemError::ENOMEM);
    };
    let started = ProcessManager::wakeup(&first).is_ok();
    let first_ready = started
        && poll_until(|| {
            if cancel {
                decided.load(Ordering::Acquire)
            } else {
                resource_wait::is_waiting_for_resource_for_test(&first, completion.wait_queue.key())
            }
        });
    let second_started = ProcessManager::wakeup(&second).is_ok();
    let second_registered = second_started
        && poll_until(|| {
            resource_wait::is_waiting_for_resource_for_test(&second, completion.wait_queue.key())
        });
    completion.complete();
    // In the counting case the first release has been consumed, but its
    // consumer is still registered. A second release selects it again. The
    // after-retirement relay must pass that second ticket to the sleeper.
    let decision = first_ready && (cancel || poll_until(|| decided.load(Ordering::Acquire)));
    if !cancel {
        completion.complete();
    }
    let selected_first = first.sched_info().state().is_runnable()
        && second.sched_info().state().is_blocked()
        && !consumed.load(Ordering::Acquire);
    release.store(true, Ordering::Release);
    let transferred = poll_until(|| consumed.load(Ordering::Acquire));
    let ticket_consumed = !completion.completion_done();
    // Rescue only after recording the production result; never call rescue
    // success. Every failure path still releases and joins both real workers.
    completion.complete_all();
    let joined_first = KernelThreadMechanism::stop(&first);
    let joined_second = KernelThreadMechanism::stop(&second);
    Ok(decision
        && selected_first
        && second_registered
        && transferred
        && ticket_consumed
        && joined_first == Ok(0)
        && joined_second == Ok(0))
}
