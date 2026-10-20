//! Exercise the real request and credit waits without touching a live connection.
use super::*;
use crate::{
    arch::ipc::signal::{SigSet, Signal},
    ipc::signal_types::{SaHandlerType, SigactionType},
    mm::oom::denial,
    mm::VirtAddr,
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        pid::PidType,
    },
    sched::completion::Completion,
    smp::cpu::{smp_cpu_manager, ProcessorId},
    time::{clocksource::HZ, timer::clock},
};
use alloc::{boxed::Box, string::ToString};

#[derive(Clone, Copy)]
enum Case {
    Ready,
    Error,
    Disconnected,
    Interrupted,
    Uninterruptible,
    CreditReleased,
    CreditConfigured,
    CreditDisconnected,
    CreditInterrupted,
}

impl Case {
    fn credit(self) -> bool {
        matches!(
            self,
            Self::CreditReleased
                | Self::CreditConfigured
                | Self::CreditDisconnected
                | Self::CreditInterrupted
        )
    }

    fn signal(self) -> bool {
        matches!(
            self,
            Self::Interrupted | Self::CreditInterrupted | Self::Uninterruptible
        )
    }
}

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

// The private kthread consumes pending signals itself; no user frame executes.
extern "C" fn handled_signal(_signal: i32) {}

fn ready_direct(outcome: FuseReadWaitOutcome) -> bool {
    matches!(
        outcome,
        FuseReadWaitOutcome::Complete(Ok(FuseReadPagesReply::Direct { bytes: 17 }))
    )
}

fn case(
    kind: Case,
    waiter_cpu: ProcessorId,
    publisher_cpu: ProcessorId,
) -> Result<bool, SystemError> {
    let background = FuseBackgroundState::new();
    background.configure(1, 1);
    let mut credit = background.acquire(false)?;
    let pending = Arc::new(FusePendingState::new_with_credit(
        1,
        super::super::protocol::FUSE_READ,
        if kind.credit() { None } else { credit.take() },
        None,
    ));
    let held = Arc::new(SpinLock::new(credit));
    // A pending signal must not hide an already terminal request.
    let already_ready = Arc::new(FusePendingState::new(2, super::super::protocol::FUSE_READ));
    already_ready.complete_read_pages_direct(17);
    let passed = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(AtomicBool::new(false));
    let rescued = Arc::new(AtomicBool::new(false));
    let completion = Arc::new(Completion::new());
    let worker_pending = pending.clone();
    let worker_background = background.clone();
    let worker_passed = passed.clone();
    let worker_done = done.clone();
    let worker_completion = completion.clone();
    let waiter = KernelThreadMechanism::create_on_cpu(
        KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                let current = ProcessManager::current_pcb();
                let original_action = current.sighand().handler(Signal::SIGUSR1).unwrap();
                let mut action = original_action;
                action.set_action(SigactionType::SaHandler(SaHandlerType::Customized(
                    VirtAddr::new(handled_signal as *const () as usize),
                )));
                current.sighand().set_handler(Signal::SIGUSR1, action);
                current
                    .sig_info_mut()
                    .sig_block_mut()
                    .remove(Signal::SIGUSR1.into_sigset());
                current.recalc_sigpending();
                let ok = {
                    let _scope = denial::Scope::enter(true);
                    let before = denial::probe_denied();
                    let outcome = if kind.credit() {
                        let speculative = worker_background
                            .acquire(true)
                            .is_ok_and(|credit| credit.is_none());
                        let result = worker_background.acquire(false);
                        speculative
                            && match kind {
                                Case::CreditReleased | Case::CreditConfigured => {
                                    result.is_ok_and(|credit| credit.is_some())
                                }
                                Case::CreditDisconnected => {
                                    matches!(result, Err(SystemError::ENOTCONN))
                                }
                                _ => matches!(result, Err(SystemError::ERESTARTSYS)),
                            }
                    } else if matches!(kind, Case::Uninterruptible) {
                        matches!(
                            worker_pending.wait_result_with_kind_uninterruptible(),
                            Ok((
                                Ok(FusePendingResult::ReadPagesDirect { bytes: 17 }),
                                FuseCompletionKind::Success
                            ))
                        )
                    } else {
                        let result = worker_pending.wait_read_pages_once();
                        match kind {
                            Case::Ready => ready_direct(result),
                            Case::Error => matches!(
                                result,
                                FuseReadWaitOutcome::Complete(Err(SystemError::EIO))
                            ),
                            Case::Disconnected => matches!(
                                result,
                                FuseReadWaitOutcome::Complete(Err(SystemError::ENOTCONN))
                            ),
                            _ => matches!(result, FuseReadWaitOutcome::Interrupted),
                        }
                    };
                    let ready_wins =
                        !kind.signal() || ready_direct(already_ready.wait_read_pages_once());
                    before && outcome && ready_wins && denial::probe_denied()
                };
                // Consume the real private test signal after ending heap denial.
                let (signal, _) = current.dequeue_pending_signal(&SigSet::empty());
                current
                    .sighand()
                    .set_handler(Signal::SIGUSR1, original_action);
                worker_passed.store(
                    ok && (!kind.signal() || signal == Signal::SIGUSR1),
                    Ordering::Release,
                );
                worker_done.store(true, Ordering::Release);
                worker_completion.complete();
                0
            }),
            (),
        )),
        "fuse-waiter".to_string(),
        waiter_cpu,
    )
    .ok_or(SystemError::ENOMEM)?;
    let target = waiter.clone();
    let publisher_pending = pending.clone();
    let publisher_background = background.clone();
    let publisher_held = held.clone();
    let publisher_done = done.clone();
    let publisher_observed = observed.clone();
    let publisher_rescued = rescued.clone();
    let publisher = KernelThreadMechanism::create_on_cpu(
        KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                let key = if kind.credit() {
                    publisher_background.wait_key()
                } else {
                    publisher_pending.wait_key()
                };
                let slept = poll(|| {
                    resource_wait::is_waiting_for_resource_for_test(&target, key)
                        && target.sched_info().state().is_blocked()
                        && !target.sched_info().is_running()
                });
                publisher_observed.store(slept, Ordering::Release);
                match kind {
                    Case::Ready => {
                        publisher_pending.complete_read_pages_direct(17);
                    }
                    Case::Error => {
                        publisher_pending.complete_daemon_error(SystemError::EIO);
                    }
                    Case::Disconnected => {
                        publisher_pending.complete_disconnected(SystemError::ENOTCONN);
                    }
                    Case::CreditReleased => {
                        let credit = publisher_held.lock_irqsave().take();
                        drop(credit);
                    }
                    Case::CreditConfigured => publisher_background.configure(2, 2),
                    Case::CreditDisconnected => publisher_background.disconnect(),
                    Case::Interrupted | Case::CreditInterrupted | Case::Uninterruptible => {
                        if Signal::SIGUSR1
                            .send_signal_info_to_pcb(None, target.clone(), PidType::PID)
                            .is_err()
                        {
                            publisher_rescued.store(true, Ordering::Release);
                        }
                        if matches!(kind, Case::Uninterruptible) {
                            // Signal delivery synchronously makes a true-mode
                            // waiter runnable. It cannot stay blocked here.
                            let still_waiting = poll(|| {
                                publisher_done.load(Ordering::Acquire)
                                    || (resource_wait::is_waiting_for_resource_for_test(
                                        &target, key,
                                    ) && target.sched_info().state().is_blocked()
                                        && !target.sched_info().is_running())
                            });
                            if !still_waiting || publisher_done.load(Ordering::Acquire) {
                                publisher_rescued.store(true, Ordering::Release);
                            }
                            publisher_pending.complete_read_pages_direct(17);
                        }
                    }
                }
                if !poll(|| publisher_done.load(Ordering::Acquire)) {
                    publisher_rescued.store(true, Ordering::Release);
                    publisher_pending.complete_disconnected(SystemError::ENOTCONN);
                    publisher_background.disconnect();
                    let _ = ProcessManager::wakeup(&target);
                }
                0
            }),
            (),
        )),
        "fuse-publisher".to_string(),
        publisher_cpu,
    );
    let Some(publisher) = publisher else {
        let _ = KernelThreadMechanism::stop(&waiter);
        return Err(SystemError::ENOMEM);
    };
    let woke =
        ProcessManager::wakeup(&publisher).is_ok() && ProcessManager::wakeup(&waiter).is_ok();
    let completed = woke
        && completion
            .wait_for_completion_timeout((15 * HZ) as i64)
            .is_ok_and(|remaining| remaining > 0);
    if !completed {
        pending.complete_disconnected(SystemError::ENOTCONN);
        background.disconnect();
        let _ = KernelThreadMechanism::request_stop(&waiter);
        let _ = KernelThreadMechanism::request_stop(&publisher);
        if poll(|| {
            waiter.sched_info().state().is_exited() && publisher.sched_info().state().is_exited()
        }) {
            let _ = KernelThreadMechanism::stop(&waiter);
            let _ = KernelThreadMechanism::stop(&publisher);
        }
        return Ok(false);
    }
    let joined = matches!(KernelThreadMechanism::stop(&publisher), Ok(0))
        && matches!(KernelThreadMechanism::stop(&waiter), Ok(0));
    // Interrupts leave the original request live until a real terminal result.
    pending.complete_disconnected(SystemError::ENOTCONN);
    let held_credit = held.lock_irqsave().take();
    drop(held_credit);
    let exact_once = !pending.complete_disconnected(SystemError::ENOTCONN)
        && background.inner.lock_irqsave().inflight == 0;
    Ok(joined
        && passed.load(Ordering::Acquire)
        && observed.load(Ordering::Acquire)
        && !rescued.load(Ordering::Acquire)
        && exact_once)
}

pub(super) fn run() -> Result<bool, SystemError> {
    let mut cpus = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .filter(|cpu| smp_cpu_manager().is_online_cpu(*cpu));
    let Some(waiter_cpu) = cpus.next() else {
        return Ok(false);
    };
    let Some(publisher_cpu) = cpus.next() else {
        return Ok(false);
    };
    for kind in [
        Case::Ready,
        Case::Error,
        Case::Disconnected,
        Case::Interrupted,
        Case::Uninterruptible,
        Case::CreditReleased,
        Case::CreditConfigured,
        Case::CreditDisconnected,
        Case::CreditInterrupted,
    ] {
        if !case(kind, waiter_cpu, publisher_cpu)? {
            return Ok(false);
        }
    }
    Ok(true)
}
