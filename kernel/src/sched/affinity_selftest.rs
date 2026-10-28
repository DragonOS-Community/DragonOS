//! Finite affinity-publication/placement regression, compiled only for sched_selftest.
use alloc::{boxed::Box, string::ToString, sync::Arc};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::{
    arch::CurrentIrqArch,
    exception::InterruptArch,
    libs::cpumask::CpuMask,
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        preempt::PreemptGuard,
        ProcessControlBlock, ProcessFlags, ProcessManager,
    },
    sched::completion::Completion,
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
    time::{clocksource::HZ, deadline::now_ns},
};

const ALL_DONE: usize = 1;
const SAFETY: usize = 2;
const ABORT: usize = 3;
const WRONG_CPU: usize = 4;
const LIMIT_NS: u64 = 3_000_000_000;

#[derive(Default)]
pub(crate) struct Report {
    pub(crate) ok: bool,
    pub(crate) release_reason: usize,
    pub(crate) release_done: usize,
    pub(crate) registered: bool,
    pub(crate) rescued: bool,
}

struct State {
    abort: AtomicBool,
    ready: AtomicBool,
    registered: AtomicBool,
    a_started: AtomicBool,
    a_done: AtomicBool,
    b_done: AtomicBool,
    a_ok: AtomicBool,
    b_ok: AtomicBool,
    release_reason: AtomicUsize,
    release_done: AtomicUsize,
    target_exit: Completion,
    a_exit: Completion,
    b_exit: Completion,
}

impl State {
    fn new() -> Self {
        Self {
            abort: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            registered: AtomicBool::new(false),
            a_started: AtomicBool::new(false),
            a_done: AtomicBool::new(false),
            b_done: AtomicBool::new(false),
            a_ok: AtomicBool::new(false),
            b_ok: AtomicBool::new(false),
            release_reason: AtomicUsize::new(0),
            release_done: AtomicUsize::new(0),
            target_exit: Completion::new(),
            a_exit: Completion::new(),
            b_exit: Completion::new(),
        }
    }
}

fn wait_poll(state: &State, mut predicate: impl FnMut() -> bool) -> bool {
    let end = now_ns().saturating_add(LIMIT_NS);
    for _ in 0..1_000_000 {
        if state.abort.load(Ordering::Acquire) {
            return false;
        }
        if predicate() {
            return true;
        }
        if now_ns() >= end {
            return false;
        }
        super::sched_yield();
    }
    false
}

fn finished(completion: &Completion) -> bool {
    matches!(completion.wait_for_completion_timeout((6 * HZ) as i64), Ok(left) if left > 0)
}

fn target(state: &State) -> i32 {
    {
        let _guard = PreemptGuard::new();
        let end = now_ns().saturating_add(LIMIT_NS);
        let mut reason = SAFETY;
        if smp_get_processor_id() != ProcessorId::new(1) {
            reason = WRONG_CPU;
        } else {
            state.ready.store(true, Ordering::Release);
            for _ in 0..1_000_000_000usize {
                if state.abort.load(Ordering::Acquire) {
                    reason = ABORT;
                    break;
                }
                if state.a_done.load(Ordering::Acquire) && state.b_done.load(Ordering::Acquire) {
                    reason = ALL_DONE;
                    break;
                }
                if now_ns() >= end {
                    break;
                }
                core::hint::spin_loop();
            }
        }
        // The oracle belongs to the pinned target, before releasing its guard.
        let done = usize::from(state.a_done.load(Ordering::Acquire))
            | (usize::from(state.b_done.load(Ordering::Acquire)) << 1);
        state.release_done.store(done, Ordering::Relaxed);
        state.release_reason.store(reason, Ordering::Release);
    }
    state.target_exit.complete();
    // A real terminal switch also releases old/scope-only placement waiters.
    0
}

fn cancel_after_registration(
    state: &State,
    target: &Arc<ProcessControlBlock>,
    a: &Arc<ProcessControlBlock>,
) -> bool {
    if !wait_poll(state, || state.ready.load(Ordering::Acquire)) {
        return false;
    }
    if state.abort.load(Ordering::Acquire) || ProcessManager::wakeup(a).is_err() {
        return false;
    }
    state.a_started.store(true, Ordering::Release);
    let tag = Arc::as_ptr(target) as usize;
    if !wait_poll(state, || {
        if !super::CPU_PLACEMENT_WAIT.has_sleeping_waiter(tag)
            || state.release_reason.load(Ordering::Acquire) != 0
        {
            return false;
        }
        let pi = target.sched_info().pi_lock_irqsave();
        target.sched_info().is_running()
            && target.sched_info().on_cpu() == Some(ProcessorId::new(1))
            && pi.cpus_allowed.iter_cpu().count() == 1
            && pi.cpus_allowed.get(ProcessorId::new(0)).unwrap_or(false)
            && target.sched_info().migrate_to() == Some(ProcessorId::new(0))
            && target.flags().contains(ProcessFlags::NEED_MIGRATE)
    }) {
        return false;
    }
    state.registered.store(true, Ordering::Release);
    crate::cgroup::cpuset::set_user_affinity(target, CpuMask::from_cpu(ProcessorId::new(1))).is_ok()
}

/// None is a platform capability skip, never a timeout converted into success.
pub(crate) fn run() -> Option<Report> {
    if !crate::smp::cpu::smp_cpu_manager_initialized() {
        return None;
    }
    let online = crate::smp::cpu::smp_cpu_manager().online_cpus();
    if !online.get(ProcessorId::new(0)).unwrap_or(false)
        || !online.get(ProcessorId::new(1)).unwrap_or(false)
    {
        return None;
    }
    if !CurrentIrqArch::is_irq_enabled() || ProcessManager::current_pcb().preempt_count() != 0 {
        return Some(Report::default());
    }
    let Ok(state) = Arc::try_new(State::new()) else {
        return Some(Report::default());
    };
    let target_state = state.clone();
    let Some(target_pcb) = KernelThreadMechanism::create(
        KernelThreadClosure::EmptyClosure((Box::new(move || target(&target_state)), ())),
        "affinity-target-selftest".to_string(),
    ) else {
        return Some(Report::default());
    };
    // Skip unavailable placement rather than silently testing another CPU set.
    let allowed = {
        let _update = crate::cgroup::lock();
        crate::cgroup::cpuset::allowed_locked(&target_pcb.task_cgroup_node())
    };
    if !allowed.get(ProcessorId::new(0)).unwrap_or(false)
        || !allowed.get(ProcessorId::new(1)).unwrap_or(false)
    {
        let _ = KernelThreadMechanism::stop(&target_pcb);
        return None;
    }
    // This target is movable. create_on_cpu would violate the per-CPU contract.
    if ProcessManager::set_cpus_allowed(&target_pcb, CpuMask::from_cpu(ProcessorId::new(1)))
        .is_err()
    {
        let _ = KernelThreadMechanism::stop(&target_pcb);
        return Some(Report::default());
    }
    let a_state = state.clone();
    let a_target = target_pcb.clone();
    let Some(a) = KernelThreadMechanism::create_on_cpu(
        KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                let ok = crate::cgroup::cpuset::set_user_affinity(
                    &a_target,
                    CpuMask::from_cpu(ProcessorId::new(0)),
                )
                .is_ok();
                a_state.a_ok.store(ok, Ordering::Relaxed);
                a_state.a_done.store(true, Ordering::Release);
                a_state.a_exit.complete();
                0
            }),
            (),
        )),
        "affinity-first-selftest".to_string(),
        ProcessorId::new(0),
    ) else {
        let _ = KernelThreadMechanism::stop(&target_pcb);
        return Some(Report::default());
    };
    let b_state = state.clone();
    let b_target = target_pcb.clone();
    let b_a = a.clone();
    let Some(b) = KernelThreadMechanism::create_on_cpu(
        KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                let ok = cancel_after_registration(&b_state, &b_target, &b_a);
                b_state.b_ok.store(ok, Ordering::Relaxed);
                b_state.b_done.store(true, Ordering::Release);
                b_state.b_exit.complete();
                0
            }),
            (),
        )),
        "affinity-cancel-selftest".to_string(),
        ProcessorId::new(0),
    ) else {
        let _ = KernelThreadMechanism::stop(&a);
        let _ = KernelThreadMechanism::stop(&target_pcb);
        return Some(Report::default());
    };
    // B must already be runnable before target can pin CPU1. A caller on
    // CPU1 may not run again until target drops its guard; B waits for ready
    // with ordinary yield, so it cannot similarly block the caller on CPU0.
    let started = ProcessManager::wakeup(&b).is_ok() && ProcessManager::wakeup(&target_pcb).is_ok();
    if !started {
        state.abort.store(true, Ordering::Release);
    }
    let target_finished = started && finished(&state.target_exit);
    state.abort.store(true, Ordering::Release);
    // Fix the verdict before cleanup. stop() alone does not prove a
    // terminal switch: its completion precedes Exited in exit teardown.
    let release_reason = state.release_reason.load(Ordering::Acquire);
    let release_done = state.release_done.load(Ordering::Relaxed);
    // Let the finite target actually return. Its terminal switch normally
    // releases old/scope-only setters, so expected failures need no rescue.
    let mut b_finished = started && finished(&state.b_exit);
    let mut a_finished = if state.a_started.load(Ordering::Acquire) {
        finished(&state.a_exit)
    } else {
        b_finished // B has returned and cannot wake A later.
    };
    let rescued = !target_finished || !b_finished || !a_finished;
    if rescued {
        // Do not call stop on a setter whose private condition is unresolved.
        // Old A may own UPDATE: use the scheduler setter, not the cpuset one.
        let mut both = CpuMask::from_cpu(ProcessorId::new(0));
        both.set(ProcessorId::new(1), true);
        let _ = ProcessManager::set_cpus_allowed(&target_pcb, both);
        super::notify_cpu_placement(&target_pcb);
        if started && !b_finished {
            b_finished = finished(&state.b_exit);
        }
        if !a_finished {
            a_finished = if state.a_started.load(Ordering::Acquire) {
                finished(&state.a_exit)
            } else {
                b_finished
            };
        }
    }
    // The target's private loop is finite/aborted; each setter now has a
    // satisfiable condition. stop has no timeout and is not the abort source.
    let target_join = KernelThreadMechanism::stop(&target_pcb);
    let a_join = KernelThreadMechanism::stop(&a);
    let b_join = KernelThreadMechanism::stop(&b);
    let registered = state.registered.load(Ordering::Acquire);
    Some(Report {
        ok: started
            && target_finished
            && b_finished
            && a_finished
            && !rescued
            && target_join == Ok(0)
            && a_join == Ok(0)
            && b_join == Ok(0)
            && registered
            && state.a_ok.load(Ordering::Acquire)
            && state.b_ok.load(Ordering::Acquire)
            && release_reason == ALL_DONE
            && release_done == 3,
        release_reason,
        release_done,
        registered,
        rescued,
    })
}
