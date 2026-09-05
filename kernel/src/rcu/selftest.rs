use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};
use core::{
    fmt::Debug,
    ptr::{self, NonNull},
    sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicUsize, Ordering},
};

use crate::{
    arch::CurrentIrqArch,
    exception::InterruptArch,
    ipc::sighand::SigHand,
    libs::{
        cpumask::CpuMask,
        notifier::{AtomicNotifierChain, NotifierBlock, NotifyResult},
        spinlock::SpinLock,
    },
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        preempt::PreemptGuard,
        ProcessManager,
    },
    sched::completion::Completion,
    smp::cpu::{smp_cpu_manager, ProcessorId},
};
use system_error::SystemError;

use super::*;

#[derive(Debug)]
struct RcuSelftestDropProbe {
    id: usize,
    drops: Arc<AtomicUsize>,
}

#[derive(Clone, Copy, Debug)]
enum RcuSelftestNotifyEvent {
    Ping,
}

type RcuSelftestAtomicNotifierChain = AtomicNotifierChain<RcuSelftestNotifyEvent, usize>;
type RcuSelftestNotifierBlock = dyn NotifierBlock<RcuSelftestNotifyEvent, usize>;

#[derive(Debug)]
struct RcuSelftestNotifier {
    id: usize,
    priority: i32,
    ret: i32,
    order: Arc<SpinLock<Vec<usize>>>,
}

impl RcuSelftestNotifier {
    fn new(id: usize, priority: i32, ret: i32, order: Arc<SpinLock<Vec<usize>>>) -> Self {
        Self {
            id,
            priority,
            ret,
            order,
        }
    }
}

impl NotifierBlock<RcuSelftestNotifyEvent, usize> for RcuSelftestNotifier {
    fn notifier_call(&self, _action: RcuSelftestNotifyEvent, data: Option<&usize>) -> i32 {
        if data != Some(&42) {
            return NotifyResult::STOP.bits();
        }

        self.order.lock_irqsave().push(self.id);
        self.ret
    }

    fn priority(&self) -> i32 {
        self.priority
    }
}

struct RcuSelftestReentrantUnregisterNotifier {
    priority: i32,
    chain: Arc<RcuSelftestAtomicNotifierChain>,
    target: SpinLock<Option<Arc<RcuSelftestNotifierBlock>>>,
    result: Arc<AtomicUsize>,
}

impl Debug for RcuSelftestReentrantUnregisterNotifier {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RcuSelftestReentrantUnregisterNotifier")
            .field("priority", &self.priority)
            .finish_non_exhaustive()
    }
}

impl NotifierBlock<RcuSelftestNotifyEvent, usize> for RcuSelftestReentrantUnregisterNotifier {
    fn notifier_call(&self, _action: RcuSelftestNotifyEvent, _data: Option<&usize>) -> i32 {
        let target = self.target.lock_irqsave().clone();
        let Some(target) = target else {
            self.result.store(3, Ordering::SeqCst);
            return NotifyResult::DONE.bits();
        };

        match self.chain.unregister(target) {
            Err(SystemError::EDEADLK_OR_EDEADLOCK) => self.result.store(1, Ordering::SeqCst),
            _ => self.result.store(3, Ordering::SeqCst),
        }

        NotifyResult::DONE.bits()
    }

    fn priority(&self) -> i32 {
        self.priority
    }
}

impl Drop for RcuSelftestDropProbe {
    fn drop(&mut self) {
        let _ = self.id;
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[repr(C)]
struct RcuSelftestCallbackProbe {
    head: RcuHead,
    hits: Arc<AtomicUsize>,
}

unsafe fn rcu_selftest_callback(head: NonNull<RcuHead>) {
    // SAFETY: `head` points to the first field of `RcuSelftestCallbackProbe`,
    // which is allocated by `Box::into_raw()` in the selftest.
    let probe = unsafe { Box::from_raw(head.as_ptr() as *mut RcuSelftestCallbackProbe) };
    probe.hits.fetch_add(1, Ordering::SeqCst);
}

fn queue_callback_probe(hits: Arc<AtomicUsize>) {
    let probe = Box::into_raw(Box::new(RcuSelftestCallbackProbe {
        head: RcuHead::new(),
        hits,
    }));
    // SAFETY: the callback owns the stable Box and reconstructs it exactly
    // once after admission has detached the embedded head.
    unsafe {
        call_rcu_raw(
            NonNull::new_unchecked(ptr::addr_of_mut!((*probe).head)),
            rcu_selftest_callback,
        );
    }
}

fn run_callback_flood_selftest() -> Result<(), &'static str> {
    let hits = Arc::new(AtomicUsize::new(0));
    let callbacks = RCU_CALLBACK_BATCH_LIMIT * 2 + 1;
    for _ in 0..callbacks {
        queue_callback_probe(hits.clone());
    }
    rcu_barrier();
    if hits.load(Ordering::SeqCst) != callbacks {
        return Err("RCU callback flood did not drain across bounded batches");
    }
    Ok(())
}

#[repr(C)]
struct RcuSlowCallbackProbe {
    head: RcuHead,
    hits: Arc<AtomicUsize>,
}

unsafe fn rcu_slow_selftest_callback(head: NonNull<RcuHead>) {
    // SAFETY: the callback owns the Box allocated below and consumes it once.
    let probe = unsafe { Box::from_raw(head.as_ptr() as *mut RcuSlowCallbackProbe) };
    let started = rcu_now();
    while !progress::elapsed_at_least(rcu_now(), started, RCU_SLOW_CALLBACK_NS + 1_000_000) {
        core::hint::spin_loop();
    }
    probe.hits.fetch_add(1, Ordering::SeqCst);
}

fn run_slow_callback_budget_selftest() -> Result<(), &'static str> {
    let slow_before = RCU_STATE.stats.slow_callbacks.load(Ordering::Acquire);
    let budget_before = RCU_STATE
        .stats
        .callback_time_budget_hits
        .load(Ordering::Acquire);
    let hits = Arc::new(AtomicUsize::new(0));
    let probe = Box::into_raw(Box::new(RcuSlowCallbackProbe {
        head: RcuHead::new(),
        hits: hits.clone(),
    }));
    // SAFETY: ownership of the stable Box transfers to the callback.
    unsafe {
        call_rcu_raw(
            NonNull::new_unchecked(ptr::addr_of_mut!((*probe).head)),
            rcu_slow_selftest_callback,
        );
    }
    rcu_barrier();

    if hits.load(Ordering::SeqCst) != 1
        || RCU_STATE.stats.slow_callbacks.load(Ordering::Acquire) <= slow_before
        || RCU_STATE
            .stats
            .callback_time_budget_hits
            .load(Ordering::Acquire)
            <= budget_before
        || RCU_STATE.stats.max_callback_ns.load(Ordering::Acquire) < RCU_SLOW_CALLBACK_NS
    {
        return Err("RCU slow callback did not trigger time-budget diagnostics");
    }
    Ok(())
}

fn run_reschedule_escalation_selftest() -> Result<(), &'static str> {
    if !crate::smp::kick_cpu_supported() {
        // The policy and failure accounting remain covered on every
        // architecture. This production-path test requires an implemented
        // generic KickCpu IPI receiver.
        return Ok(());
    }

    let current_cpu = crate::smp::core::smp_get_processor_id();
    let Some(target_cpu) = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .find(|cpu| *cpu != current_cpu && smp_cpu_manager().is_online_cpu(*cpu))
    else {
        // UP configurations cannot exercise a remote reschedule IPI.
        return Ok(());
    };

    // Finish any pre-existing GP before constructing the target holdout. The
    // success condition below is per target CPU, so unrelated GP traffic on a
    // different CPU cannot satisfy this test.
    synchronize_rcu();
    let ready = Arc::new(Completion::new());
    let done = Arc::new(Completion::new());
    let saw_escalation = Arc::new(AtomicBool::new(false));
    let thread_ready = ready.clone();
    let thread_done = done.clone();
    let thread_saw = saw_escalation.clone();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let preempt_guard = PreemptGuard::new();
            let submitted_before = RCU_STATE.contexts[target_cpu.data() as usize].resched_ipis();
            let received_before = crate::smp::kick_cpu_received(target_cpu);
            let started = rcu_now();
            thread_ready.complete();
            while !(progress::elapsed_at_least(rcu_now(), started, 3_000_000_000)
                || RCU_STATE.contexts[target_cpu.data() as usize].resched_ipis()
                    != submitted_before
                    && crate::smp::kick_cpu_received(target_cpu) != received_before)
            {
                core::hint::spin_loop();
            }
            let escalated = RCU_STATE.contexts[target_cpu.data() as usize].resched_ipis()
                != submitted_before
                && crate::smp::kick_cpu_received(target_cpu) != received_before;
            thread_saw.store(escalated, Ordering::Release);
            drop(preempt_guard);
            crate::sched::sched_yield();
            thread_done.complete();
            i32::from(!escalated)
        }),
        (),
    ));
    let Some(worker) = KernelThreadMechanism::create_on_cpu(
        closure,
        "rcu-resched-holdout".to_string(),
        target_cpu,
    ) else {
        return Err("RCU reschedule selftest could not create its holdout task");
    };
    if ProcessManager::wakeup(&worker).is_err() {
        let _ = KernelThreadMechanism::stop(&worker);
        return Err("RCU reschedule selftest could not wake its holdout task");
    }
    if ready.wait_for_completion().is_err() {
        let _ = KernelThreadMechanism::stop(&worker);
        return Err("RCU reschedule selftest holdout did not start");
    }

    synchronize_rcu();
    let done_result = done.wait_for_completion();
    let stop_result = KernelThreadMechanism::stop(&worker);
    if done_result.is_err() || stop_result.is_err() || !saw_escalation.load(Ordering::Acquire) {
        return Err("RCU holdout did not recover through reschedule escalation");
    }
    Ok(())
}

fn run_smp_callback_barrier_selftest() -> Result<(), &'static str> {
    let cpus: Vec<ProcessorId> = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .filter(|cpu| smp_cpu_manager().is_online_cpu(*cpu))
        .collect();
    if cpus.is_empty() {
        return Err("RCU SMP callback selftest found no online CPU");
    }

    const CALLBACKS_PER_CPU: usize = 9;
    let hits = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(Completion::new());
    let start = Arc::new(Completion::new());
    let first_admitted = Arc::new(Completion::new());
    let continue_enqueue = Arc::new(Completion::new());
    let done = Arc::new(Completion::new());
    let mut workers = Vec::new();

    for cpu in cpus.iter().copied() {
        let thread_hits = hits.clone();
        let thread_ready = ready.clone();
        let thread_start = start.clone();
        let thread_first_admitted = first_admitted.clone();
        let thread_continue = continue_enqueue.clone();
        let thread_done = done.clone();
        let closure = KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                thread_ready.complete();
                if thread_start.wait_for_completion().is_err() {
                    thread_done.complete();
                    return 1;
                }

                queue_callback_probe(thread_hits.clone());
                thread_first_admitted.complete();
                if thread_continue.wait_for_completion().is_err() {
                    thread_done.complete();
                    return 1;
                }
                for _ in 1..CALLBACKS_PER_CPU {
                    queue_callback_probe(thread_hits.clone());
                }
                thread_done.complete();
                0
            }),
            (),
        ));
        let Some(worker) = KernelThreadMechanism::create_on_cpu(
            closure,
            format!("rcu-callback-cpu{}", cpu.data()),
            cpu,
        ) else {
            start.complete_all();
            continue_enqueue.complete_all();
            for worker in &workers {
                let _ = KernelThreadMechanism::stop(worker);
            }
            return Err("RCU SMP callback selftest could not create a worker");
        };
        if ProcessManager::wakeup(&worker).is_err() {
            start.complete_all();
            continue_enqueue.complete_all();
            let _ = KernelThreadMechanism::stop(&worker);
            for worker in &workers {
                let _ = KernelThreadMechanism::stop(worker);
            }
            return Err("RCU SMP callback selftest could not wake a worker");
        }
        workers.push(worker);
    }

    for _ in &workers {
        if ready.wait_for_completion().is_err() {
            start.complete_all();
            continue_enqueue.complete_all();
            for worker in &workers {
                let _ = KernelThreadMechanism::stop(worker);
            }
            return Err("RCU SMP callback workers did not reach their start gate");
        }
    }
    start.complete_all();
    for _ in &workers {
        if first_admitted.wait_for_completion().is_err() {
            continue_enqueue.complete_all();
            for worker in &workers {
                let _ = KernelThreadMechanism::stop(worker);
            }
            return Err("RCU SMP callback workers did not admit their first callback");
        }
    }

    // Every CPU now owns a callback ahead of its barrier marker. Release the
    // remaining admissions immediately before the barrier to exercise its
    // ownership scan concurrently with enqueue.
    continue_enqueue.complete_all();
    rcu_barrier();
    for _ in &workers {
        if done.wait_for_completion().is_err() {
            for worker in &workers {
                let _ = KernelThreadMechanism::stop(worker);
            }
            return Err("RCU SMP callback workers did not finish enqueue");
        }
    }
    rcu_barrier();

    let mut stopped = true;
    for worker in &workers {
        stopped &= KernelThreadMechanism::stop(worker).is_ok();
    }
    if !stopped {
        return Err("RCU SMP callback selftest could not stop its workers");
    }
    if hits.load(Ordering::SeqCst) != workers.len() * CALLBACKS_PER_CPU {
        return Err("RCU SMP callback/barrier selftest lost an admitted callback");
    }
    Ok(())
}

fn run_concurrent_barrier_selftest() -> Result<(), &'static str> {
    let hits = Arc::new(AtomicUsize::new(0));
    for _ in 0..(RCU_CALLBACK_BATCH_LIMIT + 1) {
        queue_callback_probe(hits.clone());
    }

    let ready = Arc::new(Completion::new());
    let start = Arc::new(Completion::new());
    let done = Arc::new(Completion::new());
    let spawn = |name: &'static str| {
        let ready = ready.clone();
        let start = start.clone();
        let done = done.clone();
        KernelThreadMechanism::create_and_run(
            KernelThreadClosure::EmptyClosure((
                Box::new(move || {
                    ready.complete();
                    if start.wait_for_completion().is_err() {
                        done.complete();
                        return 1;
                    }
                    rcu_barrier();
                    done.complete();
                    0
                }),
                (),
            )),
            name.into(),
        )
    };

    let first = spawn("rcu-barrier-a")
        .ok_or("concurrent barrier selftest could not create its first worker")?;
    let Some(second) = spawn("rcu-barrier-b") else {
        start.complete_all();
        let _ = KernelThreadMechanism::stop(&first);
        return Err("concurrent barrier selftest could not create its second worker");
    };
    if ready.wait_for_completion().is_err() || ready.wait_for_completion().is_err() {
        start.complete_all();
        let _ = KernelThreadMechanism::stop(&first);
        let _ = KernelThreadMechanism::stop(&second);
        return Err("concurrent barrier workers did not reach their start gate");
    }
    start.complete_all();
    if done.wait_for_completion().is_err() || done.wait_for_completion().is_err() {
        let _ = KernelThreadMechanism::stop(&first);
        let _ = KernelThreadMechanism::stop(&second);
        return Err("concurrent barrier workers did not finish");
    }

    let first_stopped = KernelThreadMechanism::stop(&first).is_ok();
    let second_stopped = KernelThreadMechanism::stop(&second).is_ok();
    if !first_stopped || !second_stopped {
        return Err("concurrent barrier selftest could not stop its workers");
    }
    if hits.load(Ordering::SeqCst) != RCU_CALLBACK_BATCH_LIMIT + 1 {
        return Err("concurrent barriers returned before their callback prefixes drained");
    }
    Ok(())
}

#[repr(C)]
struct RcuSelftestRequeueProbe {
    head: RcuHead,
    hits: Arc<AtomicUsize>,
}

unsafe fn rcu_selftest_requeue_callback(head: NonNull<RcuHead>) {
    let probe = head.as_ptr().cast::<RcuSelftestRequeueProbe>();
    // SAFETY: `head` is the first field of the live boxed probe.
    let hit = unsafe { (*probe).hits.fetch_add(1, Ordering::SeqCst) };
    if hit == 0 {
        // SAFETY: callback invocation has detached and released this head, and
        // the Box remains at the same address until the second invocation.
        unsafe { call_rcu_raw(head, rcu_selftest_requeue_callback) };
    } else {
        // SAFETY: the second callback owns the Box transferred by the test.
        drop(unsafe { Box::from_raw(probe) });
    }
}

fn run_duplicate_claim_selftest() -> Result<(), &'static str> {
    let head = Arc::new(RcuHead::new());
    let successes = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(Completion::new());
    let start = Arc::new(Completion::new());
    let done = Arc::new(Completion::new());

    let spawn = |name: &'static str| {
        let head = head.clone();
        let successes = successes.clone();
        let ready = ready.clone();
        let start = start.clone();
        let done = done.clone();
        let closure = KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                ready.complete();
                if start.wait_for_completion().is_err() {
                    done.complete();
                    return 1;
                }
                if head.try_claim() {
                    successes.fetch_add(1, Ordering::SeqCst);
                }
                done.complete();
                0
            }),
            (),
        ));
        KernelThreadMechanism::create_and_run(closure, name.to_string())
    };

    let first = spawn("rcu-duplicate-claim-a")
        .ok_or("duplicate claim selftest could not create its first worker")?;
    let Some(second) = spawn("rcu-duplicate-claim-b") else {
        start.complete_all();
        let _ = KernelThreadMechanism::stop(&first);
        return Err("duplicate claim selftest could not create its second worker");
    };

    if ready.wait_for_completion().is_err() || ready.wait_for_completion().is_err() {
        start.complete_all();
        let _ = KernelThreadMechanism::stop(&first);
        let _ = KernelThreadMechanism::stop(&second);
        return Err("duplicate claim selftest workers did not reach their start gate");
    }
    start.complete_all();
    if done.wait_for_completion().is_err() || done.wait_for_completion().is_err() {
        let _ = KernelThreadMechanism::stop(&first);
        let _ = KernelThreadMechanism::stop(&second);
        return Err("duplicate claim selftest workers did not finish their claims");
    }

    let first_stopped = KernelThreadMechanism::stop(&first).is_ok();
    let second_stopped = KernelThreadMechanism::stop(&second).is_ok();
    if !first_stopped || !second_stopped {
        return Err("duplicate claim selftest could not stop its workers");
    }
    if successes.load(Ordering::SeqCst) != 1 || !head.is_queued() {
        return Err("duplicate claim did not have exactly one state transition");
    }

    Ok(())
}

fn request_selftest_gp() -> gp::RcuSequence {
    let target = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        let target = inner.gp.request_future();
        RcuState::pump_grace_periods(&mut inner);
        target
    };
    RCU_STATE.wake_state_waiters();
    RCU_STATE.wake_worker();
    target
}

fn wait_for_selftest_gp_active(target: gp::RcuSequence, required_cpu: ProcessorId) -> bool {
    let started = rcu_now();
    loop {
        let inner = RCU_STATE.inner.lock_irqsave();
        if inner.gp.current() == target
            && !inner.gp.has_completed(target)
            && inner.gp.is_waiting_for(required_cpu)
        {
            return true;
        }
        drop(inner);
        if progress::elapsed_at_least(rcu_now(), started, 1_000_000_000) {
            return false;
        }
        sched_yield();
    }
}

fn wait_for_selftest_gp(target: gp::RcuSequence) {
    RCU_STATE.state_wait.wait_until(|| {
        RCU_STATE
            .inner
            .lock_irqsave()
            .gp
            .has_completed(target)
            .then_some(())
    });
    fence(Ordering::SeqCst);
}

fn selftest_gp_completed(target: gp::RcuSequence) -> bool {
    RCU_STATE.inner.lock_irqsave().gp.has_completed(target)
}

fn selftest_remote_cpu() -> Option<ProcessorId> {
    let current = crate::smp::core::smp_get_processor_id();
    smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .find(|cpu| *cpu != current && smp_cpu_manager().is_online_cpu(*cpu))
}

fn run_sync_read_litmus_selftest() -> Result<(), &'static str> {
    let Some(reader_cpu) = selftest_remote_cpu() else {
        // The fixed schedule needs a reader to remain non-preemptible while
        // the updater and coordinator make progress on another CPU.
        return Ok(());
    };
    synchronize_rcu();

    let current_cpu = crate::smp::core::smp_get_processor_id();
    let x = Arc::new(AtomicUsize::new(0));
    let y = Arc::new(AtomicUsize::new(0));
    let allow_y = Arc::new(AtomicBool::new(false));
    let reader_entered = Arc::new(Completion::new());
    let releaser_done = Arc::new(Completion::new());
    let saw_active_gp = Arc::new(AtomicBool::new(false));
    let target_gp = Arc::new(SpinLock::new(None));

    let reader_x = x.clone();
    let reader_y = y.clone();
    let reader_allow_y = allow_y.clone();
    let reader_entered_worker = reader_entered.clone();
    let reader_closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let _guard = rcu_read_lock();
            reader_x.store(1, Ordering::Release);
            reader_entered_worker.complete();
            while !reader_allow_y.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
            reader_y.store(1, Ordering::Relaxed);
            0
        }),
        (),
    ));
    let Some(reader) = KernelThreadMechanism::create_on_cpu(
        reader_closure,
        "rcu-sync-read-reader".to_string(),
        reader_cpu,
    ) else {
        return Err("RCU+sync+read could not create its reader");
    };
    let releaser_allow_y = allow_y.clone();
    let releaser_done_worker = releaser_done.clone();
    let releaser_saw_active_gp = saw_active_gp.clone();
    let releaser_target_gp = target_gp.clone();
    let releaser_closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let target = releaser_target_gp
                .lock_irqsave()
                .expect("RCU+sync+read target GP was not published");
            let active = wait_for_selftest_gp_active(target, reader_cpu);
            releaser_saw_active_gp.store(active, Ordering::Release);
            releaser_allow_y.store(true, Ordering::Release);
            releaser_done_worker.complete();
            i32::from(!active)
        }),
        (),
    ));
    let Some(releaser) = KernelThreadMechanism::create_on_cpu(
        releaser_closure,
        "rcu-sync-read-releaser".to_string(),
        current_cpu,
    ) else {
        allow_y.store(true, Ordering::Release);
        let _ = KernelThreadMechanism::stop(&reader);
        return Err("RCU+sync+read could not create its GP releaser");
    };
    if ProcessManager::wakeup(&reader).is_err() || reader_entered.wait_for_completion().is_err() {
        allow_y.store(true, Ordering::Release);
        let _ = KernelThreadMechanism::stop(&reader);
        let _ = KernelThreadMechanism::stop(&releaser);
        return Err("RCU+sync+read reader did not enter");
    }
    let target = request_selftest_gp();
    *target_gp.lock_irqsave() = Some(target);
    if ProcessManager::wakeup(&releaser).is_err() {
        allow_y.store(true, Ordering::Release);
        let _ = KernelThreadMechanism::stop(&reader);
        let _ = KernelThreadMechanism::stop(&releaser);
        return Err("RCU+sync+read GP releaser did not start");
    }

    let x_before_gp = x.load(Ordering::Acquire);
    wait_for_selftest_gp(target);
    let y_after_gp = y.load(Ordering::Relaxed);
    let completed = releaser_done.wait_for_completion().is_ok();
    let reader_stopped = KernelThreadMechanism::stop(&reader).is_ok();
    let releaser_stopped = KernelThreadMechanism::stop(&releaser).is_ok();
    if !saw_active_gp.load(Ordering::Acquire) || !completed || !reader_stopped || !releaser_stopped
    {
        return Err("RCU+sync+read fixed schedule did not complete");
    }
    if x_before_gp != 1 || y_after_gp != 1 {
        return Err("RCU+sync+read observed x=1 after y remained zero across a GP");
    }
    Ok(())
}

#[derive(Debug)]
struct RcuSyncFreeNode {
    value: AtomicUsize,
}

fn run_sync_free_litmus_selftest() -> Result<(), &'static str> {
    let Some(reader_cpu) = selftest_remote_cpu() else {
        return Ok(());
    };
    synchronize_rcu();

    let current_cpu = crate::smp::core::smp_get_processor_id();
    let old = Arc::new(RcuSyncFreeNode {
        value: AtomicUsize::new(1),
    });
    let new = Arc::new(RcuSyncFreeNode {
        value: AtomicUsize::new(1),
    });
    let published = Arc::new(AtomicPtr::new(Arc::as_ptr(&old) as *mut RcuSyncFreeNode));
    let allow_read = Arc::new(AtomicBool::new(false));
    let reader_loaded = Arc::new(Completion::new());
    let releaser_done = Arc::new(Completion::new());
    let saw_active_gp = Arc::new(AtomicBool::new(false));
    let target_gp = Arc::new(SpinLock::new(None));
    let observed = Arc::new(AtomicUsize::new(usize::MAX));

    let reader_published = published.clone();
    let reader_allow = allow_read.clone();
    let reader_loaded_worker = reader_loaded.clone();
    let reader_observed = observed.clone();
    let reader_closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let _guard = rcu_read_lock();
            let object = rcu_dereference(&reader_published);
            reader_loaded_worker.complete();
            while !reader_allow.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
            if object.is_null() {
                reader_observed.store(usize::MAX - 1, Ordering::Release);
                return 1;
            }
            // SAFETY: the old Arc is retained by the coordinator and RCU
            // additionally protects this dereference in the tested schedule.
            reader_observed.store(
                unsafe { &*object }.value.load(Ordering::Relaxed),
                Ordering::Release,
            );
            0
        }),
        (),
    ));
    let Some(reader) = KernelThreadMechanism::create_on_cpu(
        reader_closure,
        "rcu-sync-free-reader".to_string(),
        reader_cpu,
    ) else {
        return Err("RCU+sync+free could not create its reader");
    };
    let releaser_allow_read = allow_read.clone();
    let releaser_done_worker = releaser_done.clone();
    let releaser_saw_active_gp = saw_active_gp.clone();
    let releaser_target_gp = target_gp.clone();
    let releaser_closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let target = releaser_target_gp
                .lock_irqsave()
                .expect("RCU+sync+free target GP was not published");
            let active = wait_for_selftest_gp_active(target, reader_cpu);
            releaser_saw_active_gp.store(active, Ordering::Release);
            releaser_allow_read.store(true, Ordering::Release);
            releaser_done_worker.complete();
            i32::from(!active)
        }),
        (),
    ));
    let Some(releaser) = KernelThreadMechanism::create_on_cpu(
        releaser_closure,
        "rcu-sync-free-releaser".to_string(),
        current_cpu,
    ) else {
        allow_read.store(true, Ordering::Release);
        let _ = KernelThreadMechanism::stop(&reader);
        return Err("RCU+sync+free could not create its GP releaser");
    };
    if ProcessManager::wakeup(&reader).is_err() || reader_loaded.wait_for_completion().is_err() {
        allow_read.store(true, Ordering::Release);
        let _ = KernelThreadMechanism::stop(&reader);
        let _ = KernelThreadMechanism::stop(&releaser);
        return Err("RCU+sync+free reader did not load the old pointer");
    }
    rcu_assign_pointer(&published, Arc::as_ptr(&new) as *mut RcuSyncFreeNode);
    let target = request_selftest_gp();
    *target_gp.lock_irqsave() = Some(target);
    if ProcessManager::wakeup(&releaser).is_err() {
        allow_read.store(true, Ordering::Release);
        let _ = KernelThreadMechanism::stop(&reader);
        let _ = KernelThreadMechanism::stop(&releaser);
        return Err("RCU+sync+free GP releaser did not start");
    }

    wait_for_selftest_gp(target);
    old.value.store(0, Ordering::Relaxed);
    let completed = releaser_done.wait_for_completion().is_ok();
    let reader_stopped = KernelThreadMechanism::stop(&reader).is_ok();
    let releaser_stopped = KernelThreadMechanism::stop(&releaser).is_ok();
    if !saw_active_gp.load(Ordering::Acquire) || !completed || !reader_stopped || !releaser_stopped
    {
        return Err("RCU+sync+free fixed schedule did not complete");
    }
    if observed.load(Ordering::Acquire) != 1 {
        return Err("RCU+sync+free reader observed the GP-after destruction write");
    }
    Ok(())
}

fn run_reader_handoff_selftest() -> Result<(), &'static str> {
    let Some(first_cpu) = selftest_remote_cpu() else {
        return Ok(());
    };
    synchronize_rcu();

    let coordinator_cpu = crate::smp::core::smp_get_processor_id();
    let release_first = Arc::new(AtomicBool::new(false));
    let first_entered = Arc::new(Completion::new());
    let successor_done = Arc::new(Completion::new());
    let target_gp = Arc::new(SpinLock::new(None));
    let completed_during_successor = Arc::new(AtomicBool::new(false));

    let first_release = release_first.clone();
    let first_entered_worker = first_entered.clone();
    let first_closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let _guard = rcu_read_lock();
            first_entered_worker.complete();
            while !first_release.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
            0
        }),
        (),
    ));
    let Some(first) = KernelThreadMechanism::create_on_cpu(
        first_closure,
        "rcu-reader-handoff-first".to_string(),
        first_cpu,
    ) else {
        return Err("RCU reader handoff could not create its first reader");
    };

    let successor_release = release_first.clone();
    let successor_done_worker = successor_done.clone();
    let successor_target = target_gp.clone();
    let successor_completed = completed_during_successor.clone();
    let successor_closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let _guard = rcu_read_lock();
            let target = successor_target
                .lock_irqsave()
                .expect("RCU reader handoff target GP was not published");
            // The readers overlap: the successor enters before it releases
            // the reader that was present at target-GP start.
            successor_release.store(true, Ordering::Release);
            let started = rcu_now();
            while !selftest_gp_completed(target)
                && !progress::elapsed_at_least(rcu_now(), started, 1_000_000_000)
            {
                core::hint::spin_loop();
            }
            successor_completed.store(selftest_gp_completed(target), Ordering::Release);
            successor_done_worker.complete();
            0
        }),
        (),
    ));
    let Some(successor) = KernelThreadMechanism::create_on_cpu(
        successor_closure,
        "rcu-reader-handoff-successor".to_string(),
        coordinator_cpu,
    ) else {
        release_first.store(true, Ordering::Release);
        let _ = KernelThreadMechanism::stop(&first);
        return Err("RCU reader handoff could not create its successor");
    };

    if ProcessManager::wakeup(&first).is_err() || first_entered.wait_for_completion().is_err() {
        release_first.store(true, Ordering::Release);
        let _ = KernelThreadMechanism::stop(&first);
        let _ = KernelThreadMechanism::stop(&successor);
        return Err("RCU reader handoff first reader did not enter");
    }
    let target = request_selftest_gp();
    if !wait_for_selftest_gp_active(target, first_cpu) {
        release_first.store(true, Ordering::Release);
        let _ = KernelThreadMechanism::stop(&first);
        let _ = KernelThreadMechanism::stop(&successor);
        return Err("RCU reader handoff target GP did not start");
    }
    *target_gp.lock_irqsave() = Some(target);
    if ProcessManager::wakeup(&successor).is_err() || successor_done.wait_for_completion().is_err()
    {
        release_first.store(true, Ordering::Release);
        let _ = KernelThreadMechanism::stop(&first);
        let _ = KernelThreadMechanism::stop(&successor);
        return Err("RCU reader handoff successor did not finish");
    }

    let first_stopped = KernelThreadMechanism::stop(&first).is_ok();
    let successor_stopped = KernelThreadMechanism::stop(&successor).is_ok();
    wait_for_selftest_gp(target);
    if !first_stopped || !successor_stopped || !completed_during_successor.load(Ordering::Acquire) {
        return Err("RCU target GP did not progress across overlapping reader handoff");
    }
    Ok(())
}

fn run_arc_slot_cross_thread_selftest() -> Result<(), &'static str> {
    const REPLACEMENTS: usize = 64;
    let initial_drops = Arc::new(AtomicUsize::new(0));
    let replacement_drops = Arc::new(AtomicUsize::new(0));
    let slot = Arc::new(RcuArcSlot::new(Arc::new(RcuSelftestDropProbe {
        id: 40,
        drops: initial_drops.clone(),
    })));
    let reader_loaded = Arc::new(Completion::new());
    let allow_drop = Arc::new(AtomicBool::new(false));
    let reader_ok = Arc::new(AtomicBool::new(false));

    let reader_slot = slot.clone();
    let reader_loaded_worker = reader_loaded.clone();
    let reader_allow_drop = allow_drop.clone();
    let reader_ok_worker = reader_ok.clone();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let pinned = reader_slot.load();
            reader_loaded_worker.complete();
            while !reader_allow_drop.load(Ordering::Acquire) {
                sched_yield();
            }
            reader_ok_worker.store(pinned.id == 40, Ordering::Release);
            drop(pinned);
            0
        }),
        (),
    ));
    let Some(reader) =
        KernelThreadMechanism::create_and_run(closure, "rcu-arc-slot-cross-thread".to_string())
    else {
        return Err("RcuArcSlot cross-thread test could not create its reader");
    };
    if reader_loaded.wait_for_completion().is_err() {
        allow_drop.store(true, Ordering::Release);
        let _ = KernelThreadMechanism::stop(&reader);
        return Err("RcuArcSlot cross-thread reader did not pin the initial object");
    }

    for index in 0..REPLACEMENTS {
        slot.store_deferred(Arc::new(RcuSelftestDropProbe {
            id: 41 + index,
            drops: replacement_drops.clone(),
        }));
    }
    rcu_barrier();
    if initial_drops.load(Ordering::Acquire) != 0 {
        allow_drop.store(true, Ordering::Release);
        let _ = KernelThreadMechanism::stop(&reader);
        drop(slot);
        rcu_barrier();
        return Err("RcuArcSlot dropped a cross-thread pinned snapshot early");
    }

    allow_drop.store(true, Ordering::Release);
    if KernelThreadMechanism::stop(&reader).is_err() || !reader_ok.load(Ordering::Acquire) {
        drop(slot);
        rcu_barrier();
        return Err("RcuArcSlot cross-thread reader did not retain its snapshot");
    }
    if initial_drops.load(Ordering::Acquire) != 1 {
        drop(slot);
        rcu_barrier();
        return Err("RcuArcSlot initial snapshot was not dropped after the final pin");
    }
    drop(slot);
    rcu_barrier();
    if replacement_drops.load(Ordering::Acquire) != REPLACEMENTS {
        return Err("RcuArcSlot replacements were not dropped exactly once");
    }
    Ok(())
}

fn run_pr1_selftest() -> Result<(), &'static str> {
    gp::run_state_machine_selftests()?;
    super::srcu::run_state_machine_selftests()?;
    super::srcu::run_runtime_selftests()?;
    progress::run_progress_selftests()?;
    if callback_duration_ns(100, 100 + RCU_SLOW_CALLBACK_NS) != Some(RCU_SLOW_CALLBACK_NS)
        || callback_duration_ns(0, 10).is_some()
        || callback_duration_ns(100, 99).is_some()
        || callback_duration_ns(100, 200) != Some(100)
    {
        return Err("RCU callback duration validation accepted an invalid clock sample");
    }
    if RCU_SLOW_CALLBACK_NS < RCU_CALLBACK_TIME_BUDGET_NS {
        return Err("RCU slow callback threshold is below the batch time budget");
    }
    crate::sched::clock::run_sched_clock_selftests()?;
    #[cfg(target_arch = "x86_64")]
    {
        let cycles = usize::MAX / 1_000_000 + 1;
        let khz = 3_000_000;
        let expected = ((cycles as u128 * 1_000_000u128) / khz as u128) as usize;
        if crate::arch::time::cycles_to_ns(cycles, khz) != expected {
            return Err("x86 cycles-to-nanoseconds conversion overflowed");
        }
    }
    context::run_context_selftests()?;
    callback::run_segmented_callback_selftests()?;
    run_callback_generation_selftest()?;
    run_duplicate_claim_selftest()?;
    run_cpu_hotplug_lifecycle_selftest()?;
    run_cpu_hotplug_concurrent_selftest()?;
    run_immediate_gp_accounting_selftest()?;
    run_callback_flood_selftest()?;
    run_slow_callback_budget_selftest()?;
    run_reschedule_escalation_selftest()?;
    run_smp_callback_barrier_selftest()?;
    run_concurrent_barrier_selftest()?;
    run_sync_read_litmus_selftest()?;
    run_sync_free_litmus_selftest()?;
    run_reader_handoff_selftest()?;
    run_arc_slot_cross_thread_selftest()?;

    if ProcessManager::current_pcb().rcu_read_depth() != 0 {
        return Err("initial rcu_read_depth was not zero");
    }

    {
        let _outer = rcu_read_lock();
        if ProcessManager::current_pcb().rcu_read_depth() != 1 {
            return Err("outer rcu_read_lock depth mismatch");
        }

        {
            let _inner = rcu_read_lock();
            if ProcessManager::current_pcb().rcu_read_depth() != 2 {
                return Err("nested rcu_read_lock depth mismatch");
            }
        }

        if ProcessManager::current_pcb().rcu_read_depth() != 1 {
            return Err("nested rcu_read_unlock depth mismatch");
        }
    }

    if ProcessManager::current_pcb().rcu_read_depth() != 0 {
        return Err("final rcu_read_depth was not zero");
    }

    rcu_barrier();
    let (_, completed_gp_before, completed_cb_before, queued_before, ready_before) =
        debug_snapshot();
    if queued_before != 0 || ready_before {
        return Err("rcu callback queues were not empty before blocked-reader selftest");
    }

    let blocked_hits = Arc::new(AtomicUsize::new(0));
    let blocked_result = {
        let _guard = rcu_read_lock();
        rcu_defer({
            let blocked_hits = blocked_hits.clone();
            move || {
                blocked_hits.fetch_add(1, Ordering::SeqCst);
            }
        });

        if blocked_hits.load(Ordering::SeqCst) != 0 {
            Err("rcu_defer callback ran before leaving the read-side critical section")
        } else {
            note_context_switch(&ProcessManager::current_pcb());
            let (_, completed_gp_mid, completed_cb_mid, queued_mid, ready_mid) = debug_snapshot();

            if blocked_hits.load(Ordering::SeqCst) != 0 {
                Err("context switch inside rcu_read_lock executed callback early")
            } else if completed_gp_mid != completed_gp_before {
                Err("context switch inside rcu_read_lock incorrectly completed a grace period")
            } else if completed_cb_mid != completed_cb_before {
                Err("context switch inside rcu_read_lock incorrectly completed a callback")
            } else if queued_mid != 1 || ready_mid {
                Err("context switch inside rcu_read_lock corrupted callback queue state")
            } else {
                Ok(())
            }
        }
    };

    note_context_switch(&ProcessManager::current_pcb());
    rcu_barrier();
    blocked_result?;

    if blocked_hits.load(Ordering::SeqCst) != 1 {
        return Err("callback did not execute after the blocked reader left its critical section");
    }

    rcu_barrier();

    let failed_allocation_drops = Arc::new(AtomicUsize::new(0));
    let failed_allocation_probe = RcuSelftestDropProbe {
        id: 30,
        drops: failed_allocation_drops.clone(),
    };
    if try_queue_deferred_callback_with(move || drop(failed_allocation_probe), |_deferred| Err(()))
        .is_ok()
    {
        return Err("injected deferred allocation failure unexpectedly published a callback");
    }
    if failed_allocation_drops.load(Ordering::SeqCst) != 1 {
        return Err("deferred allocation failure did not return closure ownership");
    }

    let callback_hits = Arc::new(AtomicUsize::new(0));
    let callback_probe = Box::new(RcuSelftestCallbackProbe {
        head: RcuHead::new(),
        hits: callback_hits.clone(),
    });
    let callback_probe = Box::into_raw(callback_probe);

    // SAFETY: `callback_probe` stays alive until `rcu_selftest_callback()`
    // reconstructs and consumes the allocation.
    {
        let _irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        // SAFETY: `callback_probe` remains boxed at this address until the
        // callback reconstructs it. Raw admission is allocation-free and is
        // permitted while IRQs are disabled.
        unsafe {
            call_rcu_raw(
                NonNull::new_unchecked(ptr::addr_of_mut!((*callback_probe).head)),
                rcu_selftest_callback,
            );
        }
    }

    rcu_barrier();

    if callback_hits.load(Ordering::SeqCst) != 1 {
        return Err("call_rcu callback was not executed exactly once");
    }

    let requeue_hits = Arc::new(AtomicUsize::new(0));
    let requeue_probe = Box::into_raw(Box::new(RcuSelftestRequeueProbe {
        head: RcuHead::new(),
        hits: requeue_hits.clone(),
    }));
    // SAFETY: the Box is transferred to the callback and stays at a stable
    // address through both the initial admission and callback-side requeue.
    unsafe {
        call_rcu_raw(
            NonNull::new_unchecked(ptr::addr_of_mut!((*requeue_probe).head)),
            rcu_selftest_requeue_callback,
        );
    }
    rcu_barrier();
    rcu_barrier();
    if requeue_hits.load(Ordering::SeqCst) != 2 {
        return Err("callback-side RcuHead requeue did not execute exactly twice");
    }

    let deferred_drops = Arc::new(AtomicUsize::new(0));
    rcu_defer_drop(RcuSelftestDropProbe {
        id: 1,
        drops: deferred_drops.clone(),
    });
    rcu_barrier();

    if deferred_drops.load(Ordering::SeqCst) != 1 {
        return Err("rcu_defer_drop did not run after rcu_barrier");
    }

    let fallible_deferred_drops = Arc::new(AtomicUsize::new(0));
    try_rcu_defer_drop_arc(Arc::new(RcuSelftestDropProbe {
        id: 2,
        drops: fallible_deferred_drops.clone(),
    }))
    .map_err(|_| "try_rcu_defer_drop_arc could not reserve a callback")?;
    rcu_barrier();

    if fallible_deferred_drops.load(Ordering::SeqCst) != 1 {
        return Err("try_rcu_defer_drop_arc did not run after rcu_barrier");
    }

    let completed_gp_before = RCU_STATE.inner.lock_irqsave().gp.completed();
    synchronize_rcu_noalloc();
    let completed_gp_after = RCU_STATE.inner.lock_irqsave().gp.completed();
    if !completed_gp_after.has_reached(completed_gp_before.next()) {
        return Err("synchronize_rcu_noalloc did not complete a new grace period");
    }

    let deferred_hits = Arc::new(AtomicUsize::new(0));
    rcu_defer({
        let deferred_hits = deferred_hits.clone();
        move || {
            deferred_hits.fetch_add(1, Ordering::SeqCst);
        }
    });
    rcu_barrier();

    if deferred_hits.load(Ordering::SeqCst) != 1 {
        return Err("rcu_defer closure did not run after rcu_barrier");
    }

    Ok(())
}

fn run_callback_generation_selftest() -> Result<(), &'static str> {
    unsafe fn noop_callback(_head: NonNull<RcuHead>) {}

    let cpu0 = ProcessorId::new(0);
    let cpu1 = ProcessorId::new(1);
    let mut inner = RcuStateInner::new();
    let mut callbacks = RcuSegmentedCallbacks::new();
    let first_head = RcuHead::new();
    let second_head = RcuHead::new();

    let first_gp = inner.gp.request_future();
    if inner
        .gp
        .start_requested(CpuMask::from_cpu(cpu0), [0; PerCpu::MAX_CPU_NUM as usize])
        != first_gp
    {
        return Err("callback generation selftest failed to start its first GP");
    }

    if !first_head.try_claim() || !second_head.try_claim() {
        return Err("fresh callback generation heads could not be claimed");
    }
    callbacks.enqueue(NonNull::from(&first_head), noop_callback);
    callbacks.enqueue(NonNull::from(&second_head), noop_callback);
    let expected_second_gp = first_gp.next();
    let requested_second_gp = inner.gp.request_future();
    if requested_second_gp != expected_second_gp
        || !callbacks.classify_next(requested_second_gp, true)
        || callbacks.depth().next_ready != 2
    {
        return Err("callback admitted during an active GP targeted the wrong generation");
    }
    if callbacks.has_ready() || callbacks.has_unclassified() {
        return Err("worker would spin on a future GP blocked by the active waiting mask");
    }

    if !inner.gp.report_quiescent_state(cpu0) {
        return Err("callback generation selftest could not complete its first waiting mask");
    }
    if inner.gp.complete_ready() != first_gp || callbacks.complete_gp(first_gp) {
        return Err("callbacks became ready during the GP that preceded admission");
    }
    callbacks.prepare_gp_start(expected_second_gp);
    if inner
        .gp
        .start_requested(CpuMask::from_cpu(cpu1), [0; PerCpu::MAX_CPU_NUM as usize])
        != expected_second_gp
        || !inner.gp.is_waiting_for(cpu1)
        || callbacks.depth().wait != 2
        || callbacks.has_ready()
    {
        return Err("callbacks became ready before their post-admission GP completed");
    }

    if !inner.gp.report_quiescent_state(cpu1) {
        return Err("callback generation selftest could not complete its second waiting mask");
    }
    if inner.gp.complete_ready() != expected_second_gp
        || !callbacks.complete_gp(expected_second_gp)
        || callbacks.depth().done != 2
        || !callbacks.has_ready()
    {
        return Err("callbacks did not become ready after their target GP completed");
    }

    let first_callback = callbacks.pop_ready().unwrap();
    let second_callback = callbacks.pop_ready().unwrap();
    if first_callback.head != NonNull::from(&first_head)
        || second_callback.head != NonNull::from(&second_head)
    {
        return Err("callback generation transition did not preserve admission FIFO");
    }
    if !callbacks.is_empty() {
        return Err("callback generation transition did not drain its FIFO");
    }

    Ok(())
}

fn pump_cpu_lifecycle_model(inner: &mut RcuStateInner) -> bool {
    RcuState::pump_grace_periods_with(
        inner,
        1,
        GracePeriodPumpHooks {
            waiting_snapshot: |participants: &CpuMask| {
                (participants.clone(), [0; PerCpu::MAX_CPU_NUM as usize])
            },
            credit_context: |_: &mut GracePeriodState| false,
            publish_active: |_| {},
            complete_callbacks: |_| false,
            prepare_callbacks: |_| {},
            note_gp_start: |_| {},
            note_gp_complete: |_| {},
        },
    )
}

fn run_immediate_gp_accounting_selftest() -> Result<(), &'static str> {
    let mut inner = RcuStateInner::new();
    inner.gp.request_future();
    let mut starts = 0usize;
    let mut completions = 0usize;
    RcuState::pump_grace_periods_with(
        &mut inner,
        42,
        GracePeriodPumpHooks {
            waiting_snapshot: |_: &CpuMask| (CpuMask::new(), [0; PerCpu::MAX_CPU_NUM as usize]),
            credit_context: |_: &mut GracePeriodState| false,
            publish_active: |_| {},
            complete_callbacks: |_| false,
            prepare_callbacks: |_| {},
            note_gp_start: |_| starts += 1,
            note_gp_complete: |_| completions += 1,
        },
    );
    if starts != 1 || completions != 1 || inner.gp.is_active() || inner.progress.is_some() {
        return Err("immediately completed RCU GP was not accounted exactly once");
    }
    Ok(())
}

fn run_cpu_hotplug_lifecycle_selftest() -> Result<(), &'static str> {
    let cpu = ProcessorId::new(PerCpu::MAX_CPU_NUM - 1);
    let incoming = ProcessorId::new(PerCpu::MAX_CPU_NUM - 2);
    let mut inner = RcuStateInner::new();

    if !prepare_cpu_starting_locked(&inner, cpu) {
        return Err("fresh offline CPU was not eligible for RCU preparation");
    }

    // BSP publication of Starting does not admit an AP that has not executed
    // its local RCU starting hook.
    let before_ap_runs = inner.gp.request_future();
    pump_cpu_lifecycle_model(&mut inner);
    if !inner.gp.has_completed(before_ap_runs) || inner.gp.is_waiting_for(cpu) {
        return Err("RCU waited for a Starting CPU before its AP-side hook");
    }

    cpu_starting_locked(&mut inner, cpu);
    let after_ap_runs = inner.gp.request_future();
    pump_cpu_lifecycle_model(&mut inner);
    if !inner.gp.is_waiting_for(cpu) || inner.gp.has_completed(after_ap_runs) {
        return Err("RCU did not admit a CPU after its AP-side starting hook");
    }
    if prepare_cpu_starting_locked(&inner, cpu) {
        return Err("RCU allowed an active participant to be prepared again");
    }

    // An AP becoming RCU-ready during an active GP must join only the next
    // fresh snapshot.
    if !prepare_cpu_starting_locked(&inner, incoming) {
        return Err("incoming CPU could not be prepared during an active GP");
    }
    cpu_starting_locked(&mut inner, incoming);
    if inner.gp.is_waiting_for(incoming) {
        return Err("incoming CPU was added to an already-active GP");
    }
    let next_gp = inner.gp.request_future();

    // This is the original offline/new-GP race in deterministic order: the
    // first GP observed the CPU, Dying removes both future admission and its
    // existing responsibility under the same lock, and the next GP snapshot
    // must not add it back.
    cpu_dying_locked(&mut inner, cpu);
    pump_cpu_lifecycle_model(&mut inner);
    if inner.gp.is_waiting_for(cpu)
        || !inner.gp.is_waiting_for(incoming)
        || inner.gp.has_completed(next_gp)
    {
        return Err("Dying CPU was re-admitted or incoming CPU missed the next GP");
    }

    cpu_dying_locked(&mut inner, incoming);
    pump_cpu_lifecycle_model(&mut inner);
    if !inner.gp.has_completed(next_gp) {
        return Err("CPU Dying did not complete the GP it was responsible for");
    }

    // Repeated model transitions use the same production mask operations and
    // ensure no waiting bit leaks across generations. This is not presented as
    // a substitute for a future platform-level CPU restart stress test.
    for _ in 0..32 {
        if !prepare_cpu_starting_locked(&inner, cpu) {
            return Err("completed CPU teardown left stale RCU responsibility");
        }
        cpu_starting_locked(&mut inner, cpu);
        let target = inner.gp.request_future();
        pump_cpu_lifecycle_model(&mut inner);
        if !inner.gp.is_waiting_for(cpu) {
            return Err("repeated RCU lifecycle did not admit its online CPU");
        }
        cpu_dying_locked(&mut inner, cpu);
        pump_cpu_lifecycle_model(&mut inner);
        if !inner.gp.has_completed(target) || inner.gp.is_waiting_for(cpu) {
            return Err("repeated RCU lifecycle leaked a GP holdout");
        }
    }

    Ok(())
}

fn run_cpu_hotplug_concurrent_selftest() -> Result<(), &'static str> {
    const ROUNDS: usize = 32;

    // Use an absent logical CPU so the test exercises the production global
    // lifecycle hooks without racing a real CPU's context tracker. This is a
    // concurrency test for the RCU protocol, not a claim that platform-level
    // CPU reset/restart has been implemented.
    let Some(synthetic_cpu) = (0..PerCpu::MAX_CPU_NUM)
        .rev()
        .map(ProcessorId::new)
        .find(|&cpu| !smp_cpu_manager().present_cpus().get(cpu).unwrap_or(false))
    else {
        // A machine using every representable logical CPU has no context slot
        // that a synthetic lifecycle test may safely borrow.
        return Ok(());
    };

    let round_ready = Arc::new(Completion::new());
    let round_release = Arc::new(Completion::new());
    let round_withdrawn = Arc::new(Completion::new());
    let round_advance = Arc::new(Completion::new());
    let finished = Arc::new(Completion::new());
    let failed = Arc::new(AtomicBool::new(false));
    let completed_rounds = Arc::new(AtomicUsize::new(0));
    let callback_hits = Arc::new(AtomicUsize::new(0));

    let round_ready_worker = round_ready.clone();
    let round_release_worker = round_release.clone();
    let round_withdrawn_worker = round_withdrawn.clone();
    let round_advance_worker = round_advance.clone();
    let finished_worker = finished.clone();
    let failed_worker = failed.clone();
    let completed_rounds_worker = completed_rounds.clone();
    let callback_hits_worker = callback_hits.clone();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            for _ in 0..ROUNDS {
                if !prepare_cpu_starting(synthetic_cpu) {
                    failed_worker.store(true, Ordering::Release);
                    round_ready_worker.complete_all();
                    round_withdrawn_worker.complete_all();
                    round_advance_worker.complete_all();
                    break;
                }
                cpu_starting(synthetic_cpu);

                // Exercise a real read-side section and global callback
                // admission while the lifecycle participant is visible.
                {
                    let _guard = rcu_read_lock();
                    core::hint::black_box(0usize);
                }
                rcu_defer({
                    let callback_hits = callback_hits_worker.clone();
                    move || {
                        callback_hits.fetch_add(1, Ordering::AcqRel);
                    }
                });

                // Hold the synthetic participant until the main task has
                // admitted its concurrent reader/callback workload. Explicit
                // handoff avoids relying on scheduler yield fairness.
                round_ready_worker.complete();
                if round_release_worker.wait_for_completion().is_err() {
                    failed_worker.store(true, Ordering::Release);
                    cpu_dying(synthetic_cpu);
                    round_withdrawn_worker.complete_all();
                    round_advance_worker.complete_all();
                    break;
                }
                cpu_dying(synthetic_cpu);
                completed_rounds_worker.fetch_add(1, Ordering::AcqRel);
                round_withdrawn_worker.complete();
                if round_advance_worker.wait_for_completion().is_err() {
                    failed_worker.store(true, Ordering::Release);
                    break;
                }
            }

            // Idempotent cleanup keeps a failed round from poisoning later GPs.
            cpu_dying(synthetic_cpu);
            finished_worker.complete();
            0
        }),
        (),
    ));
    let Some(worker) = KernelThreadMechanism::create_and_run(
        closure,
        "rcu-hotplug-concurrency-selftest".to_string(),
    ) else {
        return Err("RCU hotplug concurrency selftest could not create its worker");
    };

    for _ in 0..ROUNDS {
        if round_ready.wait_for_completion().is_err() {
            round_release.complete_all();
            round_advance.complete_all();
            let _ = KernelThreadMechanism::stop(&worker);
            return Err("RCU hotplug concurrency selftest missed lifecycle admission");
        }
        {
            let _guard = rcu_read_lock();
            core::hint::black_box(1usize);
        }
        rcu_defer({
            let callback_hits = callback_hits.clone();
            move || {
                callback_hits.fetch_add(1, Ordering::AcqRel);
            }
        });
        round_release.complete();
        synchronize_rcu();
        rcu_barrier();
        if round_withdrawn.wait_for_completion().is_err() {
            round_release.complete_all();
            round_advance.complete_all();
            let _ = KernelThreadMechanism::stop(&worker);
            return Err("RCU hotplug concurrency selftest missed lifecycle withdrawal");
        }
        // Do not let the next synthetic Starting race ahead of this round's
        // synchronize/barrier completion and become an unintended holdout.
        round_advance.complete();
    }

    let finished_result = finished.wait_for_completion();
    let stop_result = KernelThreadMechanism::stop(&worker);
    cpu_dying(synthetic_cpu);
    rcu_barrier();

    if finished_result.is_err() || stop_result.is_err() {
        return Err("RCU hotplug concurrency selftest worker did not finish cleanly");
    }
    if failed.load(Ordering::Acquire) {
        return Err("RCU hotplug concurrency selftest rejected a clean lifecycle round");
    }
    if completed_rounds.load(Ordering::Acquire) != ROUNDS {
        return Err("RCU hotplug concurrency selftest lost a lifecycle transition");
    }
    if callback_hits.load(Ordering::Acquire) != ROUNDS * 2 {
        return Err("RCU hotplug concurrency selftest lost or duplicated a callback");
    }

    Ok(())
}

fn run_pr2_selftest() -> Result<(), &'static str> {
    let old_drops = Arc::new(AtomicUsize::new(0));
    let new_drops = Arc::new(AtomicUsize::new(0));

    let slot = RcuArcSlot::new(Arc::new(RcuSelftestDropProbe {
        id: 1,
        drops: old_drops.clone(),
    }));
    let pinned_old = slot.load();
    if pinned_old.id != 1 {
        return Err("RcuArcSlot::load did not return the published object");
    }

    slot.store_deferred(Arc::new(RcuSelftestDropProbe {
        id: 2,
        drops: new_drops.clone(),
    }));
    rcu_barrier();

    if old_drops.load(Ordering::SeqCst) != 0 {
        return Err("old slot object dropped while a pinned Arc was still alive");
    }

    if slot.load().id != 2 {
        return Err("RcuArcSlot did not publish the replacement object");
    }

    drop(pinned_old);
    if old_drops.load(Ordering::SeqCst) != 1 {
        return Err("old slot object was not dropped after the final pin was released");
    }

    drop(slot);
    rcu_barrier();
    if new_drops.load(Ordering::SeqCst) != 1 {
        return Err("current slot object was not dropped after slot destruction grace period");
    }

    let prepared_old_drops = Arc::new(AtomicUsize::new(0));
    let prepared_new_drops = Arc::new(AtomicUsize::new(0));
    let prepared_slot = RcuArcSlot::new(Arc::new(RcuSelftestDropProbe {
        id: 11,
        drops: prepared_old_drops.clone(),
    }));
    let prepared_retire = PreparedRcuArcRetire::prepare()
        .map_err(|_| "prepared RCU retirement reservation failed")?;
    let retirement = prepared_slot.swap_prepared(
        Arc::new(RcuSelftestDropProbe {
            id: 12,
            drops: prepared_new_drops.clone(),
        }),
        prepared_retire,
    );
    if prepared_old_drops.load(Ordering::SeqCst) != 0 {
        return Err("prepared RCU swap dropped the old object before enqueue");
    }
    retirement.enqueue();
    rcu_barrier();
    if prepared_old_drops.load(Ordering::SeqCst) != 1 {
        return Err("prepared RCU retirement did not drop the old object after a grace period");
    }
    if prepared_slot.load().id != 12 {
        return Err("prepared RCU swap did not publish the replacement object");
    }
    drop(prepared_slot);
    rcu_barrier();
    if prepared_new_drops.load(Ordering::SeqCst) != 1 {
        return Err("prepared RCU replacement was not dropped after slot destruction");
    }

    let with_read_old_drops = Arc::new(AtomicUsize::new(0));
    let with_read_new_drops = Arc::new(AtomicUsize::new(0));
    let with_read_slot = RcuArcSlot::new(Arc::new(RcuSelftestDropProbe {
        id: 9,
        drops: with_read_old_drops.clone(),
    }));

    let observed_id = with_read_slot.with_read(|old| {
        with_read_slot.store_deferred(Arc::new(RcuSelftestDropProbe {
            id: 10,
            drops: with_read_new_drops.clone(),
        }));

        if with_read_old_drops.load(Ordering::SeqCst) != 0 {
            return 0;
        }

        old.id
    });
    if observed_id != 9 {
        return Err("RcuArcSlot::with_read did not pin the old snapshot during replacement");
    }

    rcu_barrier();
    if with_read_old_drops.load(Ordering::SeqCst) != 1 {
        return Err("RcuArcSlot::with_read old snapshot was not dropped after the read section");
    }

    drop(with_read_slot);
    rcu_barrier();
    if with_read_new_drops.load(Ordering::SeqCst) != 1 {
        return Err("RcuArcSlot::with_read replacement snapshot was not dropped after slot drop");
    }

    let sighand = SigHand::new();
    if sighand.is_shared() {
        return Err("fresh sighand unexpectedly reported shared");
    }

    sighand.attach_task_ref();
    if sighand.load_count() != 1 || sighand.is_shared() {
        return Err("single task sighand reference tracking is broken");
    }

    let transient_pin = sighand.clone();
    drop(transient_pin);
    if sighand.load_count() != 1 {
        return Err("temporary Arc pin changed sighand task reference count");
    }

    sighand.attach_task_ref();
    if !sighand.is_shared() {
        return Err("double-attached sighand did not report shared");
    }

    sighand.detach_task_ref();
    if sighand.is_shared() || sighand.load_count() != 1 {
        return Err("sighand detach did not restore single-task state");
    }

    sighand.detach_task_ref();
    if sighand.load_count() != 0 {
        return Err("sighand task reference count did not return to zero");
    }

    Ok(())
}

fn run_pr3_selftest() -> Result<(), &'static str> {
    let option_drops = Arc::new(AtomicUsize::new(0));
    let option_replacement_drops = Arc::new(AtomicUsize::new(0));
    let option_clear_drops = Arc::new(AtomicUsize::new(0));
    let option_race_old_drops = Arc::new(AtomicUsize::new(0));
    let option_race_new_drops = Arc::new(AtomicUsize::new(0));
    let option_drop_drops = Arc::new(AtomicUsize::new(0));
    let option_slot = RcuOptionArcSlot::new_none();
    if option_slot.load().is_some() {
        return Err("RcuOptionArcSlot::new_none did not start empty");
    }

    option_slot.store_deferred(Some(Arc::new(RcuSelftestDropProbe {
        id: 3,
        drops: option_drops.clone(),
    })));
    let pinned_option = option_slot
        .load()
        .ok_or("RcuOptionArcSlot did not publish the first object")?;
    if pinned_option.id != 3 {
        return Err("RcuOptionArcSlot loaded the wrong first object");
    }

    option_slot.store_deferred(Some(Arc::new(RcuSelftestDropProbe {
        id: 4,
        drops: option_replacement_drops.clone(),
    })));
    rcu_barrier();
    if option_drops.load(Ordering::SeqCst) != 0 {
        return Err("RcuOptionArcSlot dropped a pinned old object");
    }
    if option_slot.load().map(|value| value.id) != Some(4) {
        return Err("RcuOptionArcSlot did not publish the replacement object");
    }

    drop(pinned_option);
    if option_drops.load(Ordering::SeqCst) != 1 {
        return Err("RcuOptionArcSlot old object was not dropped after final pin");
    }

    option_slot.store_deferred(None);
    rcu_barrier();
    if option_slot.load().is_some() {
        return Err("RcuOptionArcSlot did not clear to None");
    }
    if option_replacement_drops.load(Ordering::SeqCst) != 1 {
        return Err("RcuOptionArcSlot replacement object was not dropped after clear");
    }

    option_slot.store_deferred(Some(Arc::new(RcuSelftestDropProbe {
        id: 5,
        drops: option_clear_drops.clone(),
    })));
    if !option_slot.clear_if_deferred(|value| value.id == 5) {
        return Err("RcuOptionArcSlot clear_if_deferred did not clear a matching object");
    }
    rcu_barrier();
    if option_slot.load().is_some() {
        return Err("RcuOptionArcSlot clear_if_deferred left a matching object published");
    }
    if option_clear_drops.load(Ordering::SeqCst) != 1 {
        return Err("RcuOptionArcSlot clear_if_deferred did not drop the cleared object");
    }

    option_slot.store_deferred(Some(Arc::new(RcuSelftestDropProbe {
        id: 6,
        drops: option_race_old_drops.clone(),
    })));
    let replaced_once = AtomicBool::new(false);
    let cleared = option_slot.clear_if_deferred(|value| {
        if value.id == 6 && !replaced_once.swap(true, Ordering::SeqCst) {
            option_slot.store_deferred(Some(Arc::new(RcuSelftestDropProbe {
                id: 7,
                drops: option_race_new_drops.clone(),
            })));
            return true;
        }

        value.id == 6
    });
    if cleared {
        return Err("RcuOptionArcSlot clear_if_deferred cleared after a racing replacement");
    }
    if option_slot.load().map(|value| value.id) != Some(7) {
        return Err("RcuOptionArcSlot clear_if_deferred lost the racing replacement");
    }
    rcu_barrier();
    if option_race_old_drops.load(Ordering::SeqCst) != 1 {
        return Err("RcuOptionArcSlot racing old object was not dropped");
    }
    if option_race_new_drops.load(Ordering::SeqCst) != 0 {
        return Err("RcuOptionArcSlot racing replacement was dropped unexpectedly");
    }

    option_slot.store_deferred(None);
    rcu_barrier();
    if option_race_new_drops.load(Ordering::SeqCst) != 1 {
        return Err("RcuOptionArcSlot racing replacement was not dropped after final clear");
    }

    let drop_slot = RcuOptionArcSlot::new_some(Arc::new(RcuSelftestDropProbe {
        id: 8,
        drops: option_drop_drops.clone(),
    }));
    let pinned_drop_slot = drop_slot
        .load()
        .ok_or("RcuOptionArcSlot drop test did not publish an object")?;
    drop(drop_slot);
    rcu_barrier();
    if option_drop_drops.load(Ordering::SeqCst) != 0 {
        return Err("RcuOptionArcSlot drop released an object with a live reader pin");
    }

    drop(pinned_drop_slot);
    if option_drop_drops.load(Ordering::SeqCst) != 1 {
        return Err("RcuOptionArcSlot drop object was not released after final reader pin");
    }

    run_option_slot_cross_thread_lifecycle_selftest()?;

    Ok(())
}

fn run_option_slot_cross_thread_lifecycle_selftest() -> Result<(), &'static str> {
    let pinned_drops = Arc::new(AtomicUsize::new(0));
    let replacement_drops = Arc::new(AtomicUsize::new(0));
    let slot = Arc::new(RcuOptionArcSlot::new_some(Arc::new(RcuSelftestDropProbe {
        id: 20,
        drops: pinned_drops.clone(),
    })));
    let invalid_read = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let first_read = Arc::new(Completion::new());
    let reader_gate = Arc::new(Completion::new());
    let gated_read = Arc::new(Completion::new());
    let overlap_entered = Arc::new(Completion::new());
    let overlap_start = Arc::new(Completion::new());
    let overlap_done = Arc::new(Completion::new());

    let slot_reader = slot.clone();
    let reader_gate_reader = reader_gate.clone();
    let invalid_read_reader = invalid_read.clone();
    let reads_reader = reads.clone();
    let first_read_reader = first_read.clone();
    let gated_read_reader = gated_read.clone();
    let overlap_entered_reader = overlap_entered.clone();
    let overlap_start_reader = overlap_start.clone();
    let overlap_done_reader = overlap_done.clone();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let first_snapshot = slot_reader.load();
            if first_snapshot.as_ref().map(|value| value.id) != Some(20) {
                invalid_read_reader.store(true, Ordering::Release);
            }
            reads_reader.fetch_add(1, Ordering::Release);
            first_read_reader.complete();

            if reader_gate_reader.wait_for_completion().is_err() {
                invalid_read_reader.store(true, Ordering::Release);
                gated_read_reader.complete();
                overlap_entered_reader.complete();
                overlap_done_reader.complete();
                return 1;
            }

            if let Some(value) = slot_reader.load() {
                if value.id != 20 && value.id != 21 {
                    invalid_read_reader.store(true, Ordering::Release);
                }
            }
            reads_reader.fetch_add(1, Ordering::Release);
            gated_read_reader.complete();

            drop(first_snapshot);

            if let Some(value) = slot_reader.load() {
                if value.id != 20 && value.id != 21 {
                    invalid_read_reader.store(true, Ordering::Release);
                }
            }
            reads_reader.fetch_add(1, Ordering::Release);
            overlap_entered_reader.complete();

            if overlap_start_reader.wait_for_completion().is_err() {
                invalid_read_reader.store(true, Ordering::Release);
                overlap_done_reader.complete();
                return 1;
            }

            for _ in 0..510 {
                if let Some(value) = slot_reader.load() {
                    if value.id != 20 && value.id != 21 {
                        invalid_read_reader.store(true, Ordering::Release);
                    }
                }
                reads_reader.fetch_add(1, Ordering::Release);
            }
            overlap_done_reader.complete();
            0
        }),
        (),
    ));
    let Some(reader) = KernelThreadMechanism::create_and_run(
        closure,
        "rcu-option-slot-selftest-reader".to_string(),
    ) else {
        return Err("RcuOptionArcSlot failed to create its lifecycle reader");
    };

    if first_read.wait_for_completion().is_err() {
        reader_gate.complete();
        overlap_start.complete();
        let _ = KernelThreadMechanism::stop(&reader);
        slot.store_deferred(None);
        drop(slot);
        rcu_barrier();
        return Err("RcuOptionArcSlot lifecycle reader did not start");
    }

    let mut created_replacements = 0usize;
    for index in 0..128 {
        let next = if index % 3 == 0 {
            None
        } else {
            created_replacements += 1;
            Some(Arc::new(RcuSelftestDropProbe {
                id: 20 + index % 2,
                drops: replacement_drops.clone(),
            }))
        };
        slot.store_deferred(next);
    }
    rcu_barrier();
    let pinned_survived_replacements = pinned_drops.load(Ordering::Acquire) == 0;

    let reads_before_gate = reads.load(Ordering::Acquire);
    reader_gate.complete();
    if gated_read.wait_for_completion().is_err() {
        overlap_start.complete();
        let _ = KernelThreadMechanism::stop(&reader);
        slot.store_deferred(None);
        drop(slot);
        rcu_barrier();
        return Err("RcuOptionArcSlot lifecycle reader missed its gate");
    }
    let reads_after_gate = reads.load(Ordering::Acquire);

    if overlap_entered.wait_for_completion().is_err() {
        overlap_start.complete();
        let _ = KernelThreadMechanism::stop(&reader);
        slot.store_deferred(None);
        drop(slot);
        rcu_barrier();
        return Err("RcuOptionArcSlot overlap reader did not enter");
    }
    overlap_start.complete();

    for index in 128..256 {
        let next = if index % 3 == 0 {
            None
        } else {
            created_replacements += 1;
            Some(Arc::new(RcuSelftestDropProbe {
                id: 20 + index % 2,
                drops: replacement_drops.clone(),
            }))
        };
        slot.store_deferred(next);
    }

    let overlap_result = overlap_done.wait_for_completion();
    let stop_result = KernelThreadMechanism::stop(&reader);
    slot.store_deferred(None);
    drop(slot);
    rcu_barrier();

    if stop_result.is_err() {
        return Err("RcuOptionArcSlot failed to stop its lifecycle reader");
    }
    if overlap_result.is_err() {
        return Err("RcuOptionArcSlot bounded overlap reader did not finish");
    }
    if reads_after_gate <= reads_before_gate {
        return Err("RcuOptionArcSlot lifecycle reader missed its second load");
    }
    if !pinned_survived_replacements {
        return Err("RcuOptionArcSlot dropped a cross-thread pinned snapshot");
    }
    if invalid_read.load(Ordering::Acquire) {
        return Err("RcuOptionArcSlot lifecycle reader observed an invalid value");
    }
    if reads.load(Ordering::Acquire) != 513 {
        return Err("RcuOptionArcSlot bounded overlap reader ran an unexpected load count");
    }
    if pinned_drops.load(Ordering::Acquire) != 1 {
        return Err("RcuOptionArcSlot cross-thread pinned snapshot was not dropped exactly once");
    }
    if replacement_drops.load(Ordering::Acquire) != created_replacements {
        return Err("RcuOptionArcSlot cross-thread replacements were not dropped exactly once");
    }

    Ok(())
}

fn check_notifier_order(
    order: &Arc<SpinLock<Vec<usize>>>,
    expected: &[usize],
    reason: &'static str,
) -> Result<(), &'static str> {
    let observed = order.lock_irqsave().clone();
    if observed.as_slice() != expected {
        return Err(reason);
    }

    Ok(())
}

fn clear_notifier_order(order: &Arc<SpinLock<Vec<usize>>>) {
    order.lock_irqsave().clear();
}

fn run_pr5_selftest() -> Result<(), &'static str> {
    let chain = Arc::new(RcuSelftestAtomicNotifierChain::new());
    let order = Arc::new(SpinLock::new(Vec::new()));
    let data = 42;

    let high: Arc<RcuSelftestNotifierBlock> = Arc::new(RcuSelftestNotifier::new(
        1,
        20,
        NotifyResult::OK.bits(),
        order.clone(),
    ));
    let low: Arc<RcuSelftestNotifierBlock> = Arc::new(RcuSelftestNotifier::new(
        2,
        10,
        NotifyResult::DONE.bits(),
        order.clone(),
    ));
    let same_prio: Arc<RcuSelftestNotifierBlock> = Arc::new(RcuSelftestNotifier::new(
        3,
        20,
        NotifyResult::DONE.bits(),
        order.clone(),
    ));
    let stop: Arc<RcuSelftestNotifierBlock> = Arc::new(RcuSelftestNotifier::new(
        4,
        15,
        NotifyResult::STOP.bits(),
        order.clone(),
    ));

    chain
        .register(low.clone())
        .map_err(|_| "atomic notifier failed to register the low-priority block")?;
    chain
        .register(high.clone())
        .map_err(|_| "atomic notifier failed to register the high-priority block")?;

    match chain.register(high.clone()) {
        Err(SystemError::EEXIST) => {}
        _ => return Err("atomic notifier accepted a duplicated block registration"),
    }

    match chain.register_unique_prio(same_prio.clone()) {
        Err(SystemError::EBUSY) => {}
        _ => return Err("atomic notifier accepted a duplicate unique priority"),
    }

    let (ret, nr_calls) = chain.call_chain(RcuSelftestNotifyEvent::Ping, Some(&data), None);
    if ret != NotifyResult::DONE.bits() || nr_calls != 2 {
        return Err("atomic notifier full call_chain returned the wrong result");
    }
    check_notifier_order(
        &order,
        &[1, 2],
        "atomic notifier did not dispatch in priority order",
    )?;

    clear_notifier_order(&order);
    let (ret, nr_calls) = chain.call_chain(RcuSelftestNotifyEvent::Ping, Some(&data), Some(1));
    if ret != NotifyResult::OK.bits() || nr_calls != 1 {
        return Err("atomic notifier nr_to_call did not stop after one callback");
    }
    check_notifier_order(
        &order,
        &[1],
        "atomic notifier nr_to_call dispatched the wrong callbacks",
    )?;

    chain
        .register(stop.clone())
        .map_err(|_| "atomic notifier failed to register the stop block")?;

    clear_notifier_order(&order);
    let (ret, nr_calls) = chain.call_chain(RcuSelftestNotifyEvent::Ping, Some(&data), None);
    if !NotifyResult::from_bits_truncate(ret).contains(NotifyResult::STOP_MASK)
        || ret != NotifyResult::STOP.bits()
        || nr_calls != 2
    {
        return Err("atomic notifier did not honor NOTIFY_STOP_MASK");
    }
    check_notifier_order(
        &order,
        &[1, 4],
        "atomic notifier continued after a NOTIFY_STOP result",
    )?;

    chain
        .unregister(stop.clone())
        .map_err(|_| "atomic notifier failed to unregister the stop block")?;

    clear_notifier_order(&order);
    let (ret, nr_calls) = chain.call_chain(RcuSelftestNotifyEvent::Ping, Some(&data), None);
    if ret != NotifyResult::DONE.bits() || nr_calls != 2 {
        return Err("atomic notifier unregister did not publish the replacement snapshot");
    }
    check_notifier_order(
        &order,
        &[1, 2],
        "atomic notifier still dispatched an unregistered block",
    )?;

    let reentrant_result = Arc::new(AtomicUsize::new(0));
    let reentrant = Arc::new(RcuSelftestReentrantUnregisterNotifier {
        priority: 30,
        chain: chain.clone(),
        target: SpinLock::new(None),
        result: reentrant_result.clone(),
    });
    let reentrant_block: Arc<RcuSelftestNotifierBlock> = reentrant.clone();
    *reentrant.target.lock_irqsave() = Some(reentrant_block.clone());

    chain
        .register(reentrant_block.clone())
        .map_err(|_| "atomic notifier failed to register the reentrant block")?;

    let _ = chain.call_chain(RcuSelftestNotifyEvent::Ping, Some(&data), Some(1));
    if reentrant_result.load(Ordering::SeqCst) != 1 {
        return Err("atomic notifier unregister from call_chain did not return EDEADLK");
    }

    chain
        .unregister(reentrant_block)
        .map_err(|_| "atomic notifier failed to unregister the reentrant block afterward")?;
    chain
        .unregister(high)
        .map_err(|_| "atomic notifier failed to unregister the high-priority block")?;
    chain
        .unregister(low)
        .map_err(|_| "atomic notifier failed to unregister the low-priority block")?;

    Ok(())
}

pub fn run_debug_selftests() -> String {
    let has_remote_cpu = selftest_remote_cpu().is_some();
    let pr1 = run_pr1_selftest();
    let pr2 = run_pr2_selftest();
    let pr3 = run_pr3_selftest();
    let pr5 = run_pr5_selftest();
    let overall_ok = pr1.is_ok() && pr2.is_ok() && pr3.is_ok() && pr5.is_ok();

    let mut report = String::new();
    report.push_str(if overall_ok {
        "status=ok\n"
    } else {
        "status=fail\n"
    });

    match &pr1 {
        Ok(()) => report.push_str("pr1=ok\n"),
        Err(reason) => report.push_str(&format!("pr1=fail:{reason}\n")),
    }
    report.push_str(if pr1.is_err() {
        "smp_litmus=not-completed\n"
    } else if has_remote_cpu {
        "smp_litmus=ok\n"
    } else {
        "smp_litmus=skip:no-remote-cpu\n"
    });

    match pr2 {
        Ok(()) => report.push_str("pr2=ok\n"),
        Err(reason) => report.push_str(&format!("pr2=fail:{reason}\n")),
    }

    match pr3 {
        Ok(()) => report.push_str("pr3=ok\n"),
        Err(reason) => report.push_str(&format!("pr3=fail:{reason}\n")),
    }

    match pr5 {
        Ok(()) => report.push_str("pr5=ok\n"),
        Err(reason) => report.push_str(&format!("pr5=fail:{reason}\n")),
    }

    report
}
