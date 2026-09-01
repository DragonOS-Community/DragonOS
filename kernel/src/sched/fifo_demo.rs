#![allow(dead_code)]

use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};

use crate::{
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock, ProcessManager,
    },
    sched::{completion::Completion, prio::MAX_RT_PRIO, OnRq},
    smp::cpu::{smp_cpu_manager, ProcessorId},
    time::{sleep::nanosleep, PosixTimeSpec},
};

const YIELD_ROUNDS: usize = 16;
const EVENT_COUNT: usize = YIELD_ROUNDS * 2;
const EVENT_UNSET: usize = usize::MAX;
const FIFO_PRIO: i32 = MAX_RT_PRIO - 50;

struct FifoPairState {
    first_start: Completion,
    finished: [Completion; 2],
    next_event: AtomicUsize,
    events: Vec<AtomicUsize>,
    resumed: AtomicUsize,
    results: [AtomicUsize; 2],
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

    // create_on_cpu publishes the PCB before the child has necessarily
    // finished its initial blocked switch-out. Wait for stable off-rq state
    // before changing its scheduling policy.
    pcb.sched_info().wait_until_not_running();
    assert_eq!(
        *pcb.sched_info().on_rq.lock_irqsave(),
        OnRq::None,
        "fifo demo worker did not become off-rq"
    );
    ProcessManager::set_fifo_policy(&pcb, FIFO_PRIO).expect("fifo demo failed to set FIFO policy");
    pcb
}

fn run_fifo_pair(cpu: ProcessorId) {
    let state = FifoPairState::new();
    let first = create_fifo_worker(cpu, 0, state.clone());
    let second = create_fifo_worker(cpu, 1, state.clone());

    ProcessManager::wakeup(&first).expect("fifo demo failed to wake first worker");
    ProcessManager::wakeup(&second).expect("fifo demo failed to wake second worker");

    for completion in &state.finished {
        completion
            .wait_for_completion()
            .expect("fifo demo worker completion failed");
    }

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

    for (worker, pcb) in [first, second].iter().enumerate() {
        assert_eq!(
            state.results[worker].load(Ordering::Acquire),
            0,
            "fifo demo worker reported failure"
        );
        assert_eq!(
            KernelThreadMechanism::stop(pcb),
            Ok(0),
            "fifo demo worker exited with failure"
        );
    }

    log::info!("fifo_demo status=ok cpu={}", cpu.data());
}

pub fn fifo_demo_init() {
    let cpus: Vec<ProcessorId> = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .filter(|&cpu| smp_cpu_manager().is_online_cpu(cpu))
        .take(2)
        .collect();
    assert!(!cpus.is_empty(), "fifo demo found no online CPU");

    for cpu in cpus {
        run_fifo_pair(cpu);
    }
}
