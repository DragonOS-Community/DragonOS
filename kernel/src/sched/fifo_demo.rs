#![allow(dead_code)]

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};

use crate::{
    libs::cpumask::CpuMask,
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock, ProcessFlags, ProcessManager, SchedChangeRequest,
    },
    sched::{completion::Completion, prio::MAX_RT_PRIO, LinuxSchedPolicy, OnRq, SchedClass},
    smp::{
        core::smp_get_processor_id,
        cpu::{smp_cpu_manager, ProcessorId},
    },
    time::{clocksource::HZ, sleep::nanosleep, timer::clock, PosixTimeSpec},
};

const YIELD_ROUNDS: usize = 16;
const EVENT_COUNT: usize = YIELD_ROUNDS * 2;
const EVENT_UNSET: usize = usize::MAX;
const FIFO_PRIO: i32 = MAX_RT_PRIO - 50;
const TEST_TIMEOUT_TICKS: u64 = 5 * HZ;

fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
    let start = clock();
    while clock().wrapping_sub(start) < TEST_TIMEOUT_TICKS {
        if predicate() {
            return true;
        }
        crate::sched::sched_yield();
    }
    predicate()
}

fn wait_completion(completion: &Completion) -> bool {
    completion
        .wait_for_completion_timeout(TEST_TIMEOUT_TICKS as i64)
        .is_ok_and(|remaining| remaining > 0)
}

fn wait_off_rq(pcb: &Arc<ProcessControlBlock>) -> bool {
    wait_until(|| {
        !pcb.sched_info().is_running() && *pcb.sched_info().on_rq.lock_irqsave() == OnRq::None
    })
}

fn reap_workers(workers: &[Arc<ProcessControlBlock>]) {
    for worker in workers {
        if !worker.sched_info().state().is_exited() {
            let _ = ProcessManager::set_scheduler(
                worker,
                SchedChangeRequest::Normal {
                    reset_on_fork: false,
                },
            );
            let _ = KernelThreadMechanism::request_stop(worker);
        }
    }

    assert!(
        wait_until(|| workers
            .iter()
            .all(|worker| worker.sched_info().state().is_exited())),
        "fifo demo workers did not exit before the cleanup deadline"
    );

    // Reap every worker before asserting so one failure cannot leave later
    // workers unreaped. A non-zero closure result represents a scenario
    // failure (usually its own deadline), not successful cleanup.
    let mut all_results_ok = true;
    for worker in workers {
        all_results_ok &= matches!(KernelThreadMechanism::stop(worker), Ok(0));
    }
    assert!(all_results_ok, "fifo demo worker reported failure");
}

struct FifoPairState {
    first_start: Completion,
    finished: [Completion; 2],
    next_event: AtomicUsize,
    events: Vec<AtomicUsize>,
    resumed: AtomicUsize,
    results: [AtomicUsize; 2],
    abort: AtomicBool,
}

impl FifoPairState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            first_start: Completion::new(),
            finished: [Completion::new(), Completion::new()],
            next_event: AtomicUsize::new(0),
            events: (0..EVENT_COUNT)
                .map(|_| AtomicUsize::new(EVENT_UNSET))
                .collect(),
            resumed: AtomicUsize::new(0),
            results: [AtomicUsize::new(EVENT_UNSET), AtomicUsize::new(EVENT_UNSET)],
            abort: AtomicBool::new(false),
        })
    }
}

fn worker_closure(worker: usize, state: Arc<FifoPairState>) -> KernelThreadClosure {
    let closure: Box<dyn Fn() -> i32 + Send + Sync> = Box::new(move || {
        let mut result = 0usize;

        if worker == 0 {
            if state.first_start.wait_for_completion().is_err() {
                result = 1;
            }
        } else {
            state.first_start.complete();
        }

        if result == 0 {
            for _ in 0..YIELD_ROUNDS {
                if state.abort.load(Ordering::Acquire) {
                    result = 4;
                    break;
                }
                let slot = state.next_event.fetch_add(1, Ordering::AcqRel);
                if slot >= EVENT_COUNT {
                    result = 2;
                    break;
                }
                state.events[slot].store(worker, Ordering::Release);
                crate::sched::sched_yield();
            }
        }

        if result == 0 && nanosleep(PosixTimeSpec::new(0, 5_000_000)).is_err() {
            result = 3;
        }

        if result == 0 {
            state.resumed.fetch_or(1usize << worker, Ordering::AcqRel);
        }
        state.results[worker].store(result, Ordering::Release);
        state.finished[worker].complete();
        result as i32
    });

    KernelThreadClosure::EmptyClosure((closure, ()))
}

fn create_fifo_worker(
    cpu: ProcessorId,
    worker: usize,
    state: Arc<FifoPairState>,
) -> Arc<ProcessControlBlock> {
    let name = String::from(if worker == 0 {
        "fifo_demo_a"
    } else {
        "fifo_demo_b"
    });
    let pcb = KernelThreadMechanism::create_on_cpu(worker_closure(worker, state), name, cpu)
        .expect("fifo demo failed to create worker");

    assert!(wait_off_rq(&pcb), "fifo demo worker did not become off-rq");
    ProcessManager::set_fifo_policy(&pcb, FIFO_PRIO).expect("fifo demo failed to set FIFO policy");
    pcb
}

fn run_fifo_pair(cpu: ProcessorId) {
    let state = FifoPairState::new();
    let first = create_fifo_worker(cpu, 0, state.clone());
    let second = create_fifo_worker(cpu, 1, state.clone());

    ProcessManager::wakeup(&first).expect("fifo demo failed to wake first worker");
    ProcessManager::wakeup(&second).expect("fifo demo failed to wake second worker");

    let completed = state.finished.iter().all(wait_completion);
    if !completed {
        state.abort.store(true, Ordering::Release);
        state.first_start.complete_all();
    }

    reap_workers(&[first.clone(), second.clone()]);
    assert!(completed, "fifo demo worker completion timed out");

    assert_eq!(
        state.next_event.load(Ordering::Acquire),
        EVENT_COUNT,
        "fifo demo recorded the wrong event count"
    );
    for (slot, event) in state.events.iter().enumerate() {
        let expected = if slot % 2 == 0 { 1 } else { 0 };
        assert_eq!(
            event.load(Ordering::Acquire),
            expected,
            "fifo demo same-priority yield order mismatch at event {slot}"
        );
    }
    assert_eq!(
        state.resumed.load(Ordering::Acquire),
        0b11,
        "fifo demo worker did not resume after sleeping"
    );

    for worker in 0..2 {
        assert_eq!(
            state.results[worker].load(Ordering::Acquire),
            0,
            "fifo demo worker reported failure"
        );
    }

    log::info!("fifo_demo status=ok cpu={}", cpu.data());
}

struct RemoteTransitionState {
    runner_started: AtomicBool,
    runner_release: AtomicBool,
    candidate_started: AtomicBool,
    candidate_release: Completion,
    observer_started: AtomicBool,
    abort: AtomicBool,
}

impl RemoteTransitionState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            runner_started: AtomicBool::new(false),
            runner_release: AtomicBool::new(false),
            candidate_started: AtomicBool::new(false),
            candidate_release: Completion::new(),
            observer_started: AtomicBool::new(false),
            abort: AtomicBool::new(false),
        })
    }

    fn release_all(&self) {
        self.abort.store(true, Ordering::Release);
        self.runner_release.store(true, Ordering::Release);
        self.candidate_release.complete_all();
    }
}

fn task_is_current(pcb: &Arc<ProcessControlBlock>) -> bool {
    let Some(cpu) = pcb.sched_info().on_cpu() else {
        return false;
    };
    let rq = crate::sched::cpu_rq(cpu.data() as usize);
    let (rq, _guard) = rq.self_lock();
    Arc::ptr_eq(&rq.current(), pcb)
}

fn remote_runner_closure(state: Arc<RemoteTransitionState>) -> KernelThreadClosure {
    KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            state.runner_started.store(true, Ordering::Release);
            let started = clock();
            while !state.runner_release.load(Ordering::Acquire)
                && !state.abort.load(Ordering::Acquire)
                && clock().wrapping_sub(started) < TEST_TIMEOUT_TICKS
            {
                core::hint::spin_loop();
            }
            i32::from(
                !state.runner_release.load(Ordering::Acquire)
                    && !state.abort.load(Ordering::Acquire),
            )
        }),
        (),
    ))
}

fn remote_candidate_closure(state: Arc<RemoteTransitionState>) -> KernelThreadClosure {
    KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            state.candidate_started.store(true, Ordering::Release);
            i32::from(!wait_completion(&state.candidate_release))
        }),
        (),
    ))
}

fn remote_observer_closure(state: Arc<RemoteTransitionState>) -> KernelThreadClosure {
    KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            state.observer_started.store(true, Ordering::Release);
            0
        }),
        (),
    ))
}

fn run_remote_class_transitions(cpu: ProcessorId) {
    const RUNNER_PRIO: i32 = 20;
    const OBSERVER_PRIO: i32 = 30;
    const CANDIDATE_PRIO: i32 = 10;

    let state = RemoteTransitionState::new();
    let runner = KernelThreadMechanism::create_on_cpu(
        remote_runner_closure(state.clone()),
        "fifo_change_runner".into(),
        cpu,
    )
    .expect("failed to create remote scheduler-change runner");
    let candidate = KernelThreadMechanism::create_on_cpu(
        remote_candidate_closure(state.clone()),
        "fifo_change_candidate".into(),
        cpu,
    )
    .expect("failed to create remote scheduler-change candidate");
    let observer = KernelThreadMechanism::create_on_cpu(
        remote_observer_closure(state.clone()),
        "fifo_change_observer".into(),
        cpu,
    )
    .expect("failed to create remote scheduler-change observer");
    let workers = [runner.clone(), candidate.clone(), observer.clone()];

    let mut ok = workers.iter().all(wait_off_rq);
    ok &= ProcessManager::set_fifo_policy(&runner, RUNNER_PRIO).is_ok();
    ok &= ProcessManager::set_fifo_policy(&observer, OBSERVER_PRIO).is_ok();
    ok &= ProcessManager::wakeup(&runner).is_ok();
    ok &= wait_until(|| state.runner_started.load(Ordering::Acquire));
    ok &= ProcessManager::wakeup(&candidate).is_ok();
    ok &= ProcessManager::wakeup(&observer).is_ok();
    ok &= wait_until(|| {
        *candidate.sched_info().on_rq.lock_irqsave() == OnRq::Queued
            && *observer.sched_info().on_rq.lock_irqsave() == OnRq::Queued
            && task_is_current(&runner)
    });

    let invalid_snapshot = (
        observer.sched_info().policy(),
        observer.sched_info().sched_class(),
        observer.sched_info().prio(),
        *observer.sched_info().on_rq.lock_irqsave(),
    );
    ok &= ProcessManager::set_scheduler(&observer, SchedChangeRequest::Fifo { priority: -1 })
        == Err(system_error::SystemError::EINVAL);
    ok &= invalid_snapshot
        == (
            observer.sched_info().policy(),
            observer.sched_info().sched_class(),
            observer.sched_info().prio(),
            *observer.sched_info().on_rq.lock_irqsave(),
        );

    let ipi_before = crate::smp::kick_cpu_received(cpu);
    ok &= ProcessManager::set_scheduler(
        &candidate,
        SchedChangeRequest::Fifo {
            priority: CANDIDATE_PRIO,
        },
    )
    .is_ok();
    ok &= wait_until(|| state.candidate_started.load(Ordering::Acquire));
    if crate::smp::kick_cpu_supported() {
        ok &= crate::smp::kick_cpu_received(cpu) > ipi_before;
    }

    ok &= wait_until(|| candidate.sched_info().state().is_blocked());
    ok &= ProcessManager::set_scheduler(
        &candidate,
        SchedChangeRequest::Normal {
            reset_on_fork: false,
        },
    )
    .is_ok();
    ok &= candidate.sched_info().policy() == LinuxSchedPolicy::Normal
        && candidate.sched_info().sched_class() == SchedClass::Fair
        && candidate.sched_info().prio() == candidate.sched_info().static_prio();

    ok &= wait_until(|| task_is_current(&runner));
    ok &= ProcessManager::set_scheduler(
        &runner,
        SchedChangeRequest::Normal {
            reset_on_fork: false,
        },
    )
    .is_ok();
    ok &= wait_until(|| state.observer_started.load(Ordering::Acquire));

    state.runner_release.store(true, Ordering::Release);
    state.candidate_release.complete_all();
    let exited = wait_until(|| {
        workers
            .iter()
            .all(|worker| worker.sched_info().state().is_exited())
    });
    if !exited {
        state.release_all();
    }
    reap_workers(&workers);

    assert!(ok && exited, "remote scheduler-change scenario failed");
    log::info!("fifo_demo scheduler_change_remote=ok cpu={}", cpu.data());
}

struct PriorityOrderState {
    runner_started: AtomicBool,
    runner_release: AtomicBool,
    target_started: AtomicBool,
    target_release: AtomicBool,
    peer_started: AtomicBool,
    next_event: AtomicUsize,
    events: [AtomicUsize; 2],
    abort: AtomicBool,
}

impl PriorityOrderState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            runner_started: AtomicBool::new(false),
            runner_release: AtomicBool::new(false),
            target_started: AtomicBool::new(false),
            target_release: AtomicBool::new(false),
            peer_started: AtomicBool::new(false),
            next_event: AtomicUsize::new(0),
            events: [AtomicUsize::new(EVENT_UNSET), AtomicUsize::new(EVENT_UNSET)],
            abort: AtomicBool::new(false),
        })
    }

    fn record(&self, worker: usize) -> i32 {
        let slot = self.next_event.fetch_add(1, Ordering::AcqRel);
        if slot >= self.events.len() {
            return 1;
        }
        self.events[slot].store(worker, Ordering::Release);
        0
    }

    fn release_all(&self) {
        self.abort.store(true, Ordering::Release);
        self.runner_release.store(true, Ordering::Release);
        self.target_release.store(true, Ordering::Release);
    }
}

fn gated_priority_worker(state: Arc<PriorityOrderState>, worker: usize) -> KernelThreadClosure {
    KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            match worker {
                0 => state.runner_started.store(true, Ordering::Release),
                1 => state.target_started.store(true, Ordering::Release),
                _ => state.peer_started.store(true, Ordering::Release),
            }

            let mut result = if worker == 0 { 0 } else { state.record(worker) };
            let release = if worker == 0 {
                &state.runner_release
            } else {
                &state.target_release
            };
            if worker != 2 {
                let started = clock();
                while !release.load(Ordering::Acquire)
                    && !state.abort.load(Ordering::Acquire)
                    && clock().wrapping_sub(started) < TEST_TIMEOUT_TICKS
                {
                    core::hint::spin_loop();
                }
                if !release.load(Ordering::Acquire) && !state.abort.load(Ordering::Acquire) {
                    result = 2;
                }
            }
            result
        }),
        (),
    ))
}

fn run_priority_order_changes(cpu: ProcessorId) {
    const RUNNER_PRIO: i32 = 10;
    const TARGET_PRIO: i32 = 20;
    const SHARED_PRIO: i32 = 30;
    const LOWERED_PRIO: i32 = 40;

    let state = PriorityOrderState::new();
    let runner = KernelThreadMechanism::create_on_cpu(
        gated_priority_worker(state.clone(), 0),
        "fifo_head_runner".into(),
        cpu,
    )
    .expect("failed to create head-order runner");
    let target = KernelThreadMechanism::create_on_cpu(
        gated_priority_worker(state.clone(), 1),
        "fifo_head_target".into(),
        cpu,
    )
    .expect("failed to create head-order target");
    let peer = KernelThreadMechanism::create_on_cpu(
        gated_priority_worker(state.clone(), 2),
        "fifo_head_peer".into(),
        cpu,
    )
    .expect("failed to create head-order peer");
    let workers = [runner.clone(), target.clone(), peer.clone()];

    let mut ok = workers.iter().all(wait_off_rq);
    ok &= ProcessManager::set_fifo_policy(&runner, RUNNER_PRIO).is_ok();
    ok &= ProcessManager::set_fifo_policy(&target, TARGET_PRIO).is_ok();
    ok &= ProcessManager::set_fifo_policy(&peer, SHARED_PRIO).is_ok();
    ok &= ProcessManager::wakeup(&runner).is_ok();
    ok &= wait_until(|| state.runner_started.load(Ordering::Acquire));
    ok &= ProcessManager::wakeup(&target).is_ok();
    ok &= ProcessManager::wakeup(&peer).is_ok();
    ok &= wait_until(|| {
        *target.sched_info().on_rq.lock_irqsave() == OnRq::Queued
            && *peer.sched_info().on_rq.lock_irqsave() == OnRq::Queued
            && task_is_current(&runner)
    });

    let reset_before = target.sched_info().pi_lock_irqsave().sched_reset_on_fork();
    let invalid_snapshot = (
        target.sched_info().policy(),
        target.sched_info().sched_class(),
        target.sched_info().prio(),
        *target.sched_info().on_rq.lock_irqsave(),
        reset_before,
    );
    ok &= ProcessManager::set_scheduler(
        &target,
        SchedChangeRequest::Fifo {
            priority: MAX_RT_PRIO,
        },
    ) == Err(system_error::SystemError::EINVAL);
    let reset_after = target.sched_info().pi_lock_irqsave().sched_reset_on_fork();
    ok &= invalid_snapshot
        == (
            target.sched_info().policy(),
            target.sched_info().sched_class(),
            target.sched_info().prio(),
            *target.sched_info().on_rq.lock_irqsave(),
            reset_after,
        );

    // Lower target into peer's existing bucket. Linux places it at the head.
    ok &= ProcessManager::set_scheduler(
        &target,
        SchedChangeRequest::Fifo {
            priority: SHARED_PRIO,
        },
    )
    .is_ok();
    state.runner_release.store(true, Ordering::Release);
    ok &= wait_until(|| state.target_started.load(Ordering::Acquire));
    ok &= !state.peer_started.load(Ordering::Acquire);

    // Target is current. Lower it below the queued peer and require preemption.
    ok &= ProcessManager::set_scheduler(
        &target,
        SchedChangeRequest::Fifo {
            priority: LOWERED_PRIO,
        },
    )
    .is_ok();
    ok &= wait_until(|| state.peer_started.load(Ordering::Acquire));
    ok &= !state.target_release.load(Ordering::Acquire);

    state.target_release.store(true, Ordering::Release);
    let exited = wait_until(|| {
        workers
            .iter()
            .all(|worker| worker.sched_info().state().is_exited())
    });
    if !exited {
        state.release_all();
    }
    reap_workers(&workers);

    ok &= state.next_event.load(Ordering::Acquire) == 2;
    ok &= state.events[0].load(Ordering::Acquire) == 1;
    ok &= state.events[1].load(Ordering::Acquire) == 2;
    assert!(ok && exited, "FIFO priority-order scenario failed");
    log::info!("fifo_demo scheduler_change_order=ok cpu={}", cpu.data());
}

const AFFINITY_RACE_ROUNDS: usize = 32;

struct AffinityRaceState {
    start: AtomicBool,
    abort: AtomicBool,
    target_started: AtomicBool,
    visited_cpus: AtomicUsize,
    controller_done: Completion,
    controller_result: AtomicUsize,
}

impl AffinityRaceState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            start: AtomicBool::new(false),
            abort: AtomicBool::new(false),
            target_started: AtomicBool::new(false),
            visited_cpus: AtomicUsize::new(0),
            controller_done: Completion::new(),
            controller_result: AtomicUsize::new(EVENT_UNSET),
        })
    }
}

fn affinity_target_closure(state: Arc<AffinityRaceState>) -> KernelThreadClosure {
    KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            state.target_started.store(true, Ordering::Release);
            let mut race_started = None;
            while !state.abort.load(Ordering::Acquire) {
                if state.start.load(Ordering::Acquire) {
                    let started = *race_started.get_or_insert_with(clock);
                    if clock().wrapping_sub(started) >= TEST_TIMEOUT_TICKS {
                        break;
                    }
                }
                let cpu = smp_get_processor_id().data() as usize;
                if cpu < usize::BITS as usize {
                    state.visited_cpus.fetch_or(1usize << cpu, Ordering::AcqRel);
                }
                core::hint::spin_loop();
            }
            i32::from(!state.abort.load(Ordering::Acquire))
        }),
        (),
    ))
}

fn affinity_controller_closure(
    state: Arc<AffinityRaceState>,
    target: Arc<ProcessControlBlock>,
    masks: [CpuMask; 2],
) -> KernelThreadClosure {
    KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let started = clock();
            while !state.start.load(Ordering::Acquire)
                && !state.abort.load(Ordering::Acquire)
                && clock().wrapping_sub(started) < TEST_TIMEOUT_TICKS
            {
                core::hint::spin_loop();
            }

            let mut result = usize::from(!state.start.load(Ordering::Acquire));
            if result == 0 {
                for round in 0..AFFINITY_RACE_ROUNDS {
                    if ProcessManager::set_cpus_allowed(&target, masks[round & 1].clone()).is_err()
                    {
                        result = 2;
                        break;
                    }
                    crate::sched::sched_yield();
                }
            }
            state.controller_result.store(result, Ordering::Release);
            state.controller_done.complete();
            result as i32
        }),
        (),
    ))
}

fn run_policy_affinity_race(control_cpu: ProcessorId, remote_cpus: &[ProcessorId]) {
    let first_remote = remote_cpus[0];
    let state = AffinityRaceState::new();
    let target = KernelThreadMechanism::create(
        affinity_target_closure(state.clone()),
        "fifo_affinity_target".into(),
    )
    .expect("failed to create affinity race target");
    let initial_mask = CpuMask::from_cpu(first_remote);
    let mut ok = wait_off_rq(&target);
    ok &= ProcessManager::set_cpus_allowed(&target, initial_mask.clone()).is_ok();

    let masks = if remote_cpus.len() >= 2 {
        [
            CpuMask::from_cpu(remote_cpus[0]),
            CpuMask::from_cpu(remote_cpus[1]),
        ]
    } else {
        let mut broad = CpuMask::from_cpu(first_remote);
        broad.set(control_cpu, true);
        [initial_mask.clone(), broad]
    };
    let controller = KernelThreadMechanism::create_on_cpu(
        affinity_controller_closure(state.clone(), target.clone(), masks),
        "fifo_affinity_controller".into(),
        control_cpu,
    )
    .expect("failed to create affinity race controller");
    let workers = [target.clone(), controller.clone()];
    ok &= wait_off_rq(&controller);
    ok &= ProcessManager::wakeup(&target).is_ok();
    ok &= wait_until(|| state.target_started.load(Ordering::Acquire));

    // Deterministically cover both Fair and FIFO migrated enqueue before the
    // policy/affinity race. The concurrent rounds below then stress the same
    // transactions without relying on their relative scheduling order for
    // basic migration coverage.
    if remote_cpus.len() >= 2 {
        let second_remote = remote_cpus[1];
        assert!(
            ProcessManager::set_cpus_allowed(&target, CpuMask::from_cpu(second_remote)).is_ok(),
            "Fair migration preflight affinity update failed"
        );
        assert!(
            wait_until(|| target.sched_info().on_cpu() == Some(second_remote)),
            "Fair migration preflight did not reach the destination CPU"
        );
        assert!(
            ProcessManager::set_scheduler(&target, SchedChangeRequest::Fifo { priority: 50 })
                .is_ok(),
            "FIFO migration preflight policy update failed"
        );
        assert!(
            ProcessManager::set_cpus_allowed(&target, initial_mask.clone()).is_ok(),
            "FIFO migration preflight affinity update failed"
        );
        assert!(
            wait_until(|| target.sched_info().on_cpu() == Some(first_remote)),
            "FIFO migration preflight did not reach the destination CPU"
        );
        assert!(
            ProcessManager::set_scheduler(
                &target,
                SchedChangeRequest::Normal {
                    reset_on_fork: false,
                },
            )
            .is_ok(),
            "migration preflight failed to restore the Fair policy"
        );
    }

    ok &= ProcessManager::wakeup(&controller).is_ok();
    state.start.store(true, Ordering::Release);

    for round in 0..AFFINITY_RACE_ROUNDS {
        let request = if round & 1 == 0 {
            SchedChangeRequest::Fifo { priority: 50 }
        } else {
            SchedChangeRequest::Normal {
                reset_on_fork: false,
            }
        };
        if ProcessManager::set_scheduler(&target, request).is_err() {
            ok = false;
            break;
        }
        crate::sched::sched_yield();
    }

    ok &= wait_completion(&state.controller_done);
    ok &= state.controller_result.load(Ordering::Acquire) == 0;
    ok &= ProcessManager::set_cpus_allowed(&target, initial_mask).is_ok();
    ok &= wait_until(|| target.sched_info().on_cpu() == Some(first_remote));
    ok &= ProcessManager::set_scheduler(
        &target,
        SchedChangeRequest::Normal {
            reset_on_fork: false,
        },
    )
    .is_ok();

    state.abort.store(true, Ordering::Release);
    let exited = wait_until(|| {
        workers
            .iter()
            .all(|worker| worker.sched_info().state().is_exited())
    });
    reap_workers(&workers);

    let visited = state.visited_cpus.load(Ordering::Acquire);
    ok &= visited & (1usize << first_remote.data()) != 0;
    if remote_cpus.len() >= 2 {
        ok &= visited & (1usize << remote_cpus[1].data()) != 0;
    }
    assert!(ok && exited, "policy/affinity race scenario failed");
    let mode = if remote_cpus.len() >= 2 {
        "migration"
    } else {
        "publication"
    };
    log::info!(
        "fifo_demo scheduler_change_affinity=ok mode={} remote_cpus={}",
        mode,
        remote_cpus.len()
    );
}

fn run_policy_exit_race(cpu: ProcessorId) {
    const EXIT_RACE_ROUNDS: usize = 8;
    let mut ok = true;
    let mut observed_live_exit = false;

    for round in 0..EXIT_RACE_ROUNDS {
        let worker = KernelThreadMechanism::create_on_cpu(
            KernelThreadClosure::EmptyClosure((Box::new(|| 0), ())),
            alloc::format!("fifo_exit_race_{round}"),
            cpu,
        )
        .expect("failed to create exit race worker");
        ok &= wait_off_rq(&worker);
        ok &= ProcessManager::wakeup(&worker).is_ok();
        ok &= wait_until(|| worker.flags().contains(ProcessFlags::EXITING));

        // EXITING is persistent. Require evidence that at least one
        // transaction was initiated before the final Exited state, rather
        // than accepting a sequence of post-exit lifecycle updates as a race.
        if !worker.sched_info().state().is_exited() {
            observed_live_exit = true;
        }

        // Once EXITING is visible, race several complete transactions against
        // the remaining teardown and the final Exited -> schedule transition.
        for change in 0..4 {
            let request = if change & 1 == 0 {
                SchedChangeRequest::Fifo { priority: 45 }
            } else {
                SchedChangeRequest::Normal {
                    reset_on_fork: false,
                }
            };
            ok &= ProcessManager::set_scheduler(&worker, request).is_ok();
        }
        ok &= ProcessManager::set_scheduler(
            &worker,
            SchedChangeRequest::Normal {
                reset_on_fork: false,
            },
        )
        .is_ok();
        ok &= wait_until(|| worker.sched_info().state().is_exited());
        reap_workers(&[worker]);
    }

    assert!(
        ok && observed_live_exit,
        "policy/exit race scenario did not overlap live teardown"
    );
    log::info!("fifo_demo scheduler_change_exit=ok cpu={}", cpu.data());
}

pub fn fifo_demo_init() {
    let cpus: Vec<ProcessorId> = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .filter(|&cpu| smp_cpu_manager().is_online_cpu(cpu))
        .collect();
    assert!(!cpus.is_empty(), "fifo demo found no online CPU");

    for &cpu in cpus.iter().take(2) {
        run_fifo_pair(cpu);
    }

    let control_cpu = smp_get_processor_id();
    let remote_cpus: Vec<ProcessorId> = cpus
        .iter()
        .copied()
        .filter(|&cpu| cpu != control_cpu)
        .take(2)
        .collect();
    if let Some(&remote_cpu) = remote_cpus.first() {
        run_remote_class_transitions(remote_cpu);
        run_priority_order_changes(remote_cpu);
        run_policy_affinity_race(control_cpu, &remote_cpus);
        run_policy_exit_race(remote_cpu);
    } else {
        log::info!("fifo_demo scheduler_change_remote=skip reason=no_remote_cpu");
    }
}
