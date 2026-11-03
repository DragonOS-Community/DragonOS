//! Per-CPU continuation of work left by the existing bounded softirq executor.
//! Producers keep their atomic-only raise contract. These workers are permanent;
//! CPU hotplug and migration of pending vectors are not implemented here.
use alloc::{format, sync::Arc};
use core::sync::atomic::{AtomicBool, Ordering};
use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch,
    exception::{bottom_half, InterruptArch},
    libs::{
        spinlock::SpinLock,
        wait_queue::{Waiter, Waker},
    },
    mm::percpu::PerCpu,
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock, ProcessManager,
    },
    sched::{completion::Completion, cond_resched},
    smp::{
        core::smp_get_processor_id,
        cpu::{smp_cpu_manager, ProcessorId},
    },
};

struct WorkerState {
    owner: Option<Arc<ProcessControlBlock>>,
    waker: Option<Arc<Waker>>,
    startup: Option<Result<(), SystemError>>,
}

struct Slot {
    state: SpinLock<WorkerState>,
    started: Completion,
    // True only while this bound worker cannot sleep or be scheduled away.
    // Nested IRQ exits may omit a redundant kick during this interval.
    draining: AtomicBool,
}

impl Slot {
    const fn new() -> Self {
        Self {
            state: SpinLock::new(WorkerState {
                owner: None,
                waker: None,
                startup: None,
            }),
            started: Completion::new(),
            draining: AtomicBool::new(false),
        }
    }
}

static SLOTS: [Slot; PerCpu::MAX_CPU_NUM as usize] =
    [const { Slot::new() }; PerCpu::MAX_CPU_NUM as usize];
static STARTED: AtomicBool = AtomicBool::new(false);

/// Called once from the initial task, after kthreadd and SMP scheduler startup.
pub(super) fn start() -> Result<(), SystemError> {
    if STARTED.swap(true, Ordering::AcqRel) {
        return Err(SystemError::EBUSY);
    }
    assert!(CurrentIrqArch::is_irq_enabled());
    assert_eq!(ProcessManager::current_pcb().preempt_count(), 0);
    for cpu in smp_cpu_manager().online_cpus().iter_cpu() {
        let index = cpu.data() as usize;
        let slot = &SLOTS[index];
        let closure =
            KernelThreadClosure::StaticUsizeClosure((&(worker as fn(usize) -> i32), index));
        let Some(owner) =
            KernelThreadMechanism::create_on_cpu(closure, format!("softirqd/{}", index), cpu)
        else {
            log::warn!(
                "softirqd/{} creation failed; retaining IRQ/BH service",
                index
            );
            continue;
        };
        slot.state.lock_irqsave().owner = Some(owner.clone());
        // A wake failure is fatal to initialization through the caller's Result;
        // never wait for a startup acknowledgement from an unstarted task.
        ProcessManager::wakeup(&owner)?;
        slot.started.wait_for_completion()?;
        let startup = slot
            .state
            .lock_irqsave()
            .startup
            .clone()
            .expect("softirqd acknowledgement without result");
        if let Err(error) = startup {
            // Failed workers publish Err and then return, without a live waker.
            // Join before releasing the owner, and never drop it under the slot.
            KernelThreadMechanism::stop(&owner)?;
            let retired = slot.state.lock_irqsave().owner.take();
            drop(retired);
            log::warn!(
                "softirqd/{} preparation failed: {:?}; retaining IRQ/BH service",
                index,
                error
            );
        }
    }
    Ok(())
}

fn publish_failure(slot: &Slot, error: SystemError) -> i32 {
    slot.state.lock_irqsave().startup = Some(Err(error));
    slot.started.complete_all();
    1
}

fn worker(index: usize) -> i32 {
    let slot = &SLOTS[index];
    let cpu = ProcessorId::new(index as u32);
    if smp_get_processor_id() != cpu
        || !CurrentIrqArch::is_irq_enabled()
        || ProcessManager::current_pcb().preempt_count() != 0
        || bottom_half::is_local_bh_disabled()
        || super::in_softirq()
    {
        return publish_failure(slot, SystemError::EINVAL);
    }
    let (waiter, waker) = match Waiter::try_new_pair() {
        Ok(pair) => pair,
        Err(error) => return publish_failure(slot, error),
    };
    {
        let mut state = slot.state.lock_irqsave();
        state.waker = Some(waker);
        state.startup = Some(Ok(()));
    }
    slot.started.complete_all();
    // Register before checking pending: early raises need not have a worker yet.
    loop {
        assert_eq!(smp_get_processor_id(), cpu);
        assert!(CurrentIrqArch::is_irq_enabled());
        assert_eq!(ProcessManager::current_pcb().preempt_count(), 0);
        assert!(!bottom_half::is_local_bh_disabled() && !super::in_softirq());
        if super::softirq_vectors()
            .pending_for(cpu)
            .load(Ordering::SeqCst)
            == 0
        {
            waiter.wait(false).expect("permanent softirqd wait failed");
            continue;
        }
        let irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
        slot.draining.store(true, Ordering::Release);
        super::do_softirq();
        // Must precede restoring IRQs, cond_resched, and any wait preparation.
        // A current-but-already-Blocked worker still needs a real Waker kick.
        slot.draining.store(false, Ordering::Release);
        drop(irq);
        cond_resched();
    }
}

/// Only the safe residual exit calls this, with IRQs disabled and no handler,
/// table, timer-owner or scheduler lock retained. Never call from raise_softirq.
pub(super) fn kick_local() {
    let slot = &SLOTS[smp_get_processor_id().data() as usize];
    if slot.draining.load(Ordering::Acquire) {
        return;
    }
    let waker = slot.state.lock_irqsave().waker.clone();
    if let Some(waker) = waker {
        // Slot lock is gone; permanent owner/waker pins exclude terminal drops.
        waker.wake();
    }
}

/// Scheduler accounting identity, independent of the notification suppression
/// flag: nested IRQ exits can run softirqs anywhere on the daemon task.
pub(super) fn is_current_worker() -> bool {
    let irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
    let current = ProcessManager::current_pcb();
    let slot = &SLOTS[smp_get_processor_id().data() as usize];
    let is_worker = {
        let state = slot.state.lock();
        state
            .owner
            .as_ref()
            .is_some_and(|owner| Arc::ptr_eq(owner, &current))
    };
    // Do not retire a temporary task reference while holding the slot lock.
    drop(current);
    drop(irq);
    is_worker
}
