//! Fixed-owner monotonic deadline events, sharing the architecture clockevent
//! with the ordinary scheduler tick. This is not a userspace hrtimer API.
use alloc::{boxed::Box, sync::Arc};
use core::{
    alloc::AllocError,
    fmt::Debug,
    sync::atomic::{AtomicBool, Ordering},
};

#[path = "deadline_queue.rs"]
mod queue;
use queue::{DeadlineQueue, Node};
use system_error::SystemError;

use super::{
    clocksource::HZ,
    tick_common::{tick_handle_global, tick_handle_periodic},
    timekeeping,
};
use crate::{
    arch::{interrupt::TrapFrame, CurrentIrqArch},
    exception::InterruptArch,
    libs::spinlock::SpinLock,
    mm::percpu::PerCpu,
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
};

const TICK_NS: u64 = 1_000_000_000 / HZ;
const MAX_CALLBACKS: usize = 32;
// Regression-only count of ready-queue programming attempts, including fallback.
// Ordinary kernels have no counter or accounting on this path.
#[cfg(feature = "mm_selftest")]
static PROGRAM_ATTEMPTS: [core::sync::atomic::AtomicU64; PerCpu::MAX_CPU_NUM as usize] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PerCpu::MAX_CPU_NUM as usize];
static CONTINUOUS: AtomicBool = AtomicBool::new(false);
static CAPABLE: [AtomicBool; PerCpu::MAX_CPU_NUM as usize] =
    [const { AtomicBool::new(false) }; PerCpu::MAX_CPU_NUM as usize];
static QUEUES: [SpinLock<Queue>; PerCpu::MAX_CPU_NUM as usize] =
    [const { SpinLock::new(Queue::new()) }; PerCpu::MAX_CPU_NUM as usize];

type Key = (u64, usize);
#[derive(Debug)]
struct Pending {
    event: Arc<DeadlineEvent>,
    generation: u64,
}
struct Queue {
    ready: bool,
    next_tick: u64,
    pending: DeadlineQueue,
}
impl Queue {
    const fn new() -> Self {
        Self {
            ready: false,
            next_tick: 0,
            pending: DeadlineQueue::new(),
        }
    }

    /// The periodic tick shares the clockevent with queued deadlines.
    fn next_deadline(&self) -> u64 {
        self.pending
            .first_key()
            .map_or(self.next_tick, |key| key.0.min(self.next_tick))
    }
}

/// The callback runs on the event owner with IRQs disabled, without queue or
/// event-state locks. It must not sleep. Returning a deadline rearms the same
/// generation, unless an explicit arm/cancel happened during the callback.
pub trait DeadlineCallback: Debug + Send + Sync {
    fn expire(&self, now_ns: u64) -> Option<u64>;
}

#[derive(Debug)]
struct State {
    generation: u64,
    queued: Option<Key>,
    spare: Option<Box<Node>>,
}
#[derive(Debug)]
pub struct DeadlineEvent {
    owner: ProcessorId,
    callback: Arc<dyn DeadlineCallback>,
    state: SpinLock<State>,
}

impl DeadlineEvent {
    pub fn new(owner: ProcessorId, callback: Arc<dyn DeadlineCallback>) -> Arc<Self> {
        Self::try_new(owner, callback).expect("deadline event allocation failed")
    }

    pub fn try_new(
        owner: ProcessorId,
        callback: Arc<dyn DeadlineCallback>,
    ) -> Result<Arc<Self>, AllocError> {
        let spare = Node::try_new()?;
        Arc::try_new(Self {
            owner,
            callback,
            state: SpinLock::new(State {
                generation: 0,
                queued: None,
                spare: Some(spare),
            }),
        })
    }

    pub fn arm(self: &Arc<Self>, deadline_ns: u64) -> Result<(), SystemError> {
        // Existing users must be able to restart after clocksource downgrade.
        // Admission requiring sub-tick precision checks supported() separately;
        // already admitted work still receives periodic-tick fallback service.
        if !cfg!(any(target_arch = "x86_64", target_arch = "riscv64"))
            || self.owner.data() as usize >= QUEUES.len()
        {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if self.owner != smp_get_processor_id() && !remote_supported() {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let mut queue = QUEUES[self.owner.data() as usize].lock_irqsave();
        if !queue.ready {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let previous_deadline = queue.next_deadline();
        let local = self.owner == smp_get_processor_id();
        let mut state = self.state.lock_irqsave();
        let mut node = if let Some(key) = state.queued.take() {
            queue
                .pending
                .remove(&key)
                .expect("queued deadline node missing")
        } else {
            state.spare.take().expect("idle deadline spare missing")
        };
        state.generation = state.generation.wrapping_add(1);
        let key = (deadline_ns, Arc::as_ptr(self) as usize);
        node.key = key;
        if let Some(pending) = node.pending.as_mut() {
            pending.generation = state.generation;
        } else {
            node.pending = Some(Pending {
                event: self.clone(),
                generation: state.generation,
            });
        }
        queue.pending.insert(node);
        state.queued = Some(key);
        let reprogram = previous_deadline != queue.next_deadline();
        if reprogram && local {
            program_locked(&queue);
        }
        drop(state);
        drop(queue);
        // Unlocking can permit migration. Use the remote decision made while
        // pinned by the owner queue guard, even if we now run on the owner.
        if reprogram && !local {
            notify_owner(self.owner);
        }
        Ok(())
    }

    /// Cancels queued/rearm work; a callback already entered may finish.
    pub fn cancel(&self) {
        let Some(owner_queue) = QUEUES.get(self.owner.data() as usize) else {
            // Such an event can never arm; cancellation remains a safe no-op.
            return;
        };
        let mut queue = owner_queue.lock_irqsave();
        let previous_deadline = queue.next_deadline();
        let local = self.owner == smp_get_processor_id();
        let mut state = self.state.lock_irqsave();
        state.generation = state.generation.wrapping_add(1);
        let Some(key) = state.queued.take() else {
            return;
        };
        let mut node = queue
            .pending
            .remove(&key)
            .expect("queued deadline node missing");
        let retired = node.pending.take();
        assert!(state.spare.is_none());
        state.spare = Some(node);
        let reprogram = previous_deadline != queue.next_deadline();
        if reprogram && local {
            program_locked(&queue);
        }
        drop(state);
        drop(queue);
        drop(retired);
        if reprogram && !local {
            notify_owner(self.owner);
        }
    }
}

pub fn now_ns() -> u64 {
    let ts = timekeeping::monotonic_now();
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}

pub fn supported() -> bool {
    #[cfg(target_arch = "riscv64")]
    if crate::smp::cpu::smp_cpu_manager_initialized()
        && crate::smp::cpu::smp_cpu_manager().present_cpus_count() > 1
    {
        // Generic remote clockevent IPIs are not implemented on this backend.
        return false;
    }
    cfg!(any(target_arch = "x86_64", target_arch = "riscv64"))
        && CONTINUOUS.load(Ordering::Acquire)
        && CAPABLE[smp_get_processor_id().data() as usize].load(Ordering::Acquire)
}
const fn remote_supported() -> bool {
    cfg!(target_arch = "x86_64")
}

/// New sub-tick consumers may execute on any online CPU. Local fallback is
/// still allowed for already admitted events after a capability downgrade.
pub(crate) fn all_online_supported() -> bool {
    supported()
        && crate::smp::cpu::smp_cpu_manager()
            .online_cpus()
            .iter_cpu()
            .all(|cpu| CAPABLE[cpu.data() as usize].load(Ordering::Acquire))
}

/// Called after dropping the timekeeper write lock. An event deadline must
/// never be driven by jiffies itself: that would stop the tick which advances it.
pub(crate) fn clocksource_changed(continuous: bool) {
    // Keep skip-self notifications and the final local reprogram on one CPU.
    let _pin = crate::process::preempt::PreemptGuard::new();
    CONTINUOUS.store(continuous, Ordering::Release);
    for (index, slot) in QUEUES.iter().enumerate() {
        let ready = slot.lock_irqsave().ready;
        if ready {
            kick_owner(ProcessorId::new(index as u32));
        }
    }
    reprogram_local();
}

pub fn init_local() {
    let cpu = smp_get_processor_id();
    #[cfg(target_arch = "x86_64")]
    let capable = crate::arch::driver::apic::apic_timer::deadline_capable();
    #[cfg(target_arch = "riscv64")]
    let capable = crate::arch::time::riscv_time_base_freq() > 0;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
    let capable = false;
    CAPABLE[cpu.data() as usize].store(capable, Ordering::Release);
    timekeeping::init_jiffies_tick();
    let mut queue = QUEUES[cpu.data() as usize].lock_irqsave();
    queue.next_tick = now_ns().saturating_add(TICK_NS);
    queue.ready = true;
    program_locked(&queue);
}

pub fn reprogram_local() {
    let queue = QUEUES[smp_get_processor_id().data() as usize].lock_irqsave();
    if queue.ready {
        program_locked(&queue);
    }
}

fn program_locked(queue: &Queue) {
    if !queue.ready {
        return;
    }
    #[cfg(feature = "mm_selftest")]
    PROGRAM_ATTEMPTS[smp_get_processor_id().data() as usize].fetch_add(1, Ordering::Relaxed);
    if !supported() {
        #[cfg(target_arch = "x86_64")]
        crate::arch::driver::apic::apic_timer::restore_periodic();
        #[cfg(target_arch = "riscv64")]
        crate::driver::clocksource::timer_riscv::program_deadline_delta(TICK_NS);
        return;
    }
    let next = queue.next_deadline();
    let delta = next.saturating_sub(now_ns()).max(1);
    #[cfg(target_arch = "x86_64")]
    crate::arch::driver::apic::apic_timer::program_deadline_delta(delta);
    #[cfg(target_arch = "riscv64")]
    crate::driver::clocksource::timer_riscv::program_deadline_delta(delta);
}

/// Caller must be pinned through its subsequent local reprogram.
fn kick_owner(owner: ProcessorId) {
    if owner != smp_get_processor_id() {
        notify_owner(owner);
    }
}

/// Do not recheck the caller CPU: a remote producer can migrate after unlock.
fn notify_owner(owner: ProcessorId) {
    #[cfg(not(target_arch = "x86_64"))]
    let _ = owner;
    #[cfg(target_arch = "x86_64")]
    crate::arch::interrupt::ipi::send_ipi(
        crate::exception::ipi::IpiKind::SpecVector(crate::exception::HardwareIrqNumber::new(205)),
        crate::exception::ipi::IpiTarget::Specified(owner),
    );
}

pub fn handle_irq(frame: &TrapFrame) {
    let cpu = smp_get_processor_id();
    let fallback = !supported();
    let tick = {
        let mut queue = QUEUES[cpu.data() as usize].lock_irqsave();
        let now = now_ns();
        if fallback {
            true
        } else if now >= queue.next_tick {
            // Tick classification is sampled once, not guessed for the missed
            // interval after a long IRQ-disabled section.
            let periods = now.saturating_sub(queue.next_tick) / TICK_NS + 1;
            queue.next_tick = queue
                .next_tick
                .saturating_add(periods.saturating_mul(TICK_NS));
            true
        } else {
            false
        }
    };
    tick_handle_global(tick);
    if tick {
        tick_handle_periodic(frame);
    }
    if fallback {
        QUEUES[cpu.data() as usize].lock_irqsave().next_tick = now_ns().saturating_add(TICK_NS);
    }
    for _ in 0..MAX_CALLBACKS {
        let now = now_ns();
        let pending = {
            let mut queue = QUEUES[cpu.data() as usize].lock_irqsave();
            if queue.pending.first_key().is_none_or(|key| key.0 > now) {
                None
            } else {
                let node = queue.pending.pop_first().unwrap();
                Some(restore_spare(node))
            }
        };
        let Some((key, pending)) = pending else {
            break;
        };
        dispatch_pending(key, pending, now);
    }
    reprogram_local();
}

/// Caller holds the owner queue. Restore the single spare before releasing it,
/// so a remote arm can immediately reuse the node even while dispatch is pending.
fn restore_spare(mut node: Box<Node>) -> (Key, Pending) {
    let key = node.key;
    let pending = node
        .pending
        .take()
        .expect("queued deadline payload missing");
    {
        let mut state = pending.event.state.lock_irqsave();
        assert_eq!(state.queued, Some(key));
        assert_eq!(state.generation, pending.generation);
        assert!(state.spare.is_none());
        state.queued = None;
        state.spare = Some(node);
    }
    (key, pending)
}

fn dispatch_pending(_key: Key, pending: Pending, now: u64) {
    let event = pending.event;
    {
        let state = event.state.lock_irqsave();
        if state.generation != pending.generation {
            return;
        }
    }
    finish_callback(&event, pending.generation, now);
}

fn finish_callback(event: &Arc<DeadlineEvent>, generation: u64, now: u64) {
    if let Some(deadline) = event.callback.expire(now) {
        let mut queue = QUEUES[event.owner.data() as usize].lock_irqsave();
        let mut state = event.state.lock_irqsave();
        if state.generation != generation || state.queued.is_some() {
            return;
        }
        let key = (deadline, Arc::as_ptr(event) as usize);
        let mut node = state.spare.take().expect("idle deadline spare missing");
        node.key = key;
        node.pending = Some(Pending {
            event: event.clone(),
            generation,
        });
        queue.pending.insert(node);
        state.queued = Some(key);
    }
}

/// Production generation and queue primitives, without changing interrupt
/// delivery or relying on a userspace test syscall.
pub fn run_selftests() {
    #[cfg(feature = "mm_selftest")]
    deadline_queue_selftest();
    use alloc::sync::Weak;
    use core::sync::atomic::AtomicU64;
    #[derive(Debug)]
    struct Probe {
        calls: AtomicU64,
        cancel: AtomicBool,
        arm: AtomicBool,
        event: SpinLock<Weak<DeadlineEvent>>,
    }
    impl DeadlineCallback for Probe {
        fn expire(&self, now: u64) -> Option<u64> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.cancel.load(Ordering::Relaxed) {
                let event = self.event.lock_irqsave().upgrade();
                event.unwrap().cancel();
            }
            if self.arm.load(Ordering::Relaxed) {
                let event = self.event.lock_irqsave().upgrade();
                event.unwrap().arm(now.saturating_add(2 * TICK_NS)).unwrap();
            }
            Some(now.saturating_add(TICK_NS))
        }
    }
    let _preempt = crate::process::preempt::PreemptGuard::new();
    if supported() {
        let probe = Arc::new(Probe {
            calls: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            arm: AtomicBool::new(false),
            event: SpinLock::new(Weak::new()),
        });
        let event = DeadlineEvent::new(smp_get_processor_id(), probe.clone());
        *probe.event.lock_irqsave() = Arc::downgrade(&event);
        let irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
        #[cfg(feature = "mm_selftest")]
        nonhead_reprogram_selftest(&event);
        let node_identity = state_spare_identity(&event);
        let now = now_ns();
        let deadline = now.saturating_add(TICK_NS);
        event
            .arm(deadline)
            .expect("deadline selftest owner unavailable");
        let key = event.state.lock_irqsave().queued.unwrap();
        let popped = QUEUES[event.owner.data() as usize]
            .lock_irqsave()
            .pending
            .remove(&key)
            .map(|node| restore_spare(node).1)
            .unwrap();
        event.cancel();

        dispatch_pending(key, popped, deadline);
        assert_eq!(
            probe.calls.load(Ordering::Relaxed),
            0,
            "cancel must suppress a popped generation"
        );

        event.arm(deadline).unwrap();
        let key = event.state.lock_irqsave().queued.unwrap();
        let popped = QUEUES[event.owner.data() as usize]
            .lock_irqsave()
            .pending
            .remove(&key)
            .map(|node| restore_spare(node).1)
            .unwrap();
        dispatch_pending(key, popped, deadline);
        assert_eq!(probe.calls.load(Ordering::Relaxed), 1);
        assert!(
            event.state.lock_irqsave().queued.is_some(),
            "callback deadline must rearm"
        );
        event.cancel();

        // Pop atomically returns the spare. A new explicit arm can reuse it
        // before dispatch; the old popped generation must not call/rearm.
        event.arm(deadline).unwrap();
        let key = event.state.lock_irqsave().queued.unwrap();
        let popped = QUEUES[event.owner.data() as usize]
            .lock_irqsave()
            .pending
            .remove(&key)
            .map(|node| restore_spare(node).1)
            .unwrap();
        event.arm(deadline.saturating_add(TICK_NS)).unwrap();
        dispatch_pending(key, popped, deadline);
        assert_eq!(probe.calls.load(Ordering::Relaxed), 1);
        event.cancel();

        probe.arm.store(true, Ordering::Relaxed);
        event.arm(deadline).unwrap();
        let key = event.state.lock_irqsave().queued.unwrap();
        let popped = QUEUES[event.owner.data() as usize]
            .lock_irqsave()
            .pending
            .remove(&key)
            .map(|node| restore_spare(node).1)
            .unwrap();
        dispatch_pending(key, popped, deadline);
        assert_eq!(probe.calls.load(Ordering::Relaxed), 2);
        assert_eq!(
            event.state.lock_irqsave().queued.unwrap().0,
            deadline.saturating_add(2 * TICK_NS),
            "explicit callback arm must defeat automatic old rearm"
        );
        event.cancel();
        probe.arm.store(false, Ordering::Relaxed);

        probe.cancel.store(true, Ordering::Relaxed);
        event.arm(deadline).unwrap();
        let key = event.state.lock_irqsave().queued.unwrap();
        let popped = QUEUES[event.owner.data() as usize]
            .lock_irqsave()
            .pending
            .remove(&key)
            .map(|node| restore_spare(node).1)
            .unwrap();
        dispatch_pending(key, popped, deadline);
        assert_eq!(probe.calls.load(Ordering::Relaxed), 3);
        assert!(
            event.state.lock_irqsave().queued.is_none(),
            "callback cancel must prevent old rearm"
        );
        assert!(!QUEUES[event.owner.data() as usize]
            .lock_irqsave()
            .pending
            .contains_key(&key));
        assert_eq!(state_spare_identity(&event), node_identity);
        reprogram_local();
        drop(irq);
    }
}

/// Tests the public remote-owner contract, not IPI ordering or exact latency.
/// Keep this separate from run_selftests(), whose local tests pin their caller.
#[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
pub(crate) fn run_remote_owner_selftest() -> Option<bool> {
    use crate::{
        process::{
            kthread::{KernelThreadClosure, KernelThreadMechanism},
            preempt::PreemptGuard,
            ProcessManager,
        },
        sched::completion::Completion,
        smp::cpu::{smp_cpu_manager, smp_cpu_manager_initialized},
    };
    use alloc::string::ToString;
    use core::sync::atomic::{AtomicU64, AtomicUsize};

    #[derive(Debug)]
    struct ResultState {
        callback_done: Completion,
        worker_done: Completion,
        abort: AtomicBool,
        calls: AtomicUsize,
        cpu: AtomicUsize,
        when: AtomicU64,
    }
    // No event/PCB reference: late callbacks retain only their own result data.
    impl DeadlineCallback for ResultState {
        fn expire(&self, now: u64) -> Option<u64> {
            self.cpu
                .store(smp_get_processor_id().data() as usize, Ordering::Relaxed);
            self.when.store(now, Ordering::Relaxed);
            self.calls.fetch_add(1, Ordering::Release);
            self.callback_done.complete();
            None
        }
    }

    fn exercise(event: &Arc<DeadlineEvent>, result: &ResultState, producer: ProcessorId) -> bool {
        if smp_get_processor_id() != producer
            || producer == event.owner
            || result.abort.load(Ordering::Acquire)
            || KernelThreadMechanism::should_stop(&ProcessManager::current_pcb())
        {
            return false;
        }
        {
            let _pin = PreemptGuard::new();
            let generation = event.state.lock_irqsave().generation;
            if event.arm(u64::MAX - 2).is_err() || event.arm(u64::MAX - 1).is_err() {
                return false;
            }
            event.cancel();
            let state = event.state.lock_irqsave();
            if state.queued.is_some()
                || state.generation != generation.wrapping_add(3)
                || result.calls.load(Ordering::Acquire) != 0
            {
                return false;
            }
        }
        for expected in 1..=2 {
            if result.abort.load(Ordering::Acquire)
                || KernelThreadMechanism::should_stop(&ProcessManager::current_pcb())
            {
                return false;
            }
            let deadline = {
                let _pin = PreemptGuard::new();
                if smp_get_processor_id() != producer {
                    return false;
                }
                let deadline = now_ns().saturating_add(100_000);
                if event.arm(deadline).is_err() {
                    return false;
                }
                deadline
            };
            // No pin or queue/rq lock may span this sleep.
            if !matches!(result.callback_done.wait_for_completion_timeout(HZ as i64), Ok(left) if left > 0)
                || result.abort.load(Ordering::Acquire)
                || result.calls.load(Ordering::Acquire) != expected
                || result.cpu.load(Ordering::Relaxed) != event.owner.data() as usize
                || result.when.load(Ordering::Relaxed) < deadline
            {
                return false;
            }
        }
        true
    }

    if !smp_cpu_manager_initialized() || !all_online_supported() {
        return None;
    }
    let online = smp_cpu_manager().online_cpus();
    let mut cpus = online.iter_cpu();
    let owner = cpus.next()?;
    let producer = cpus.next()?;
    if !CurrentIrqArch::is_irq_enabled() || ProcessManager::current_pcb().preempt_count() != 0 {
        return Some(false);
    }
    let Ok(result) = Arc::try_new(ResultState {
        callback_done: Completion::new(),
        worker_done: Completion::new(),
        abort: AtomicBool::new(false),
        calls: AtomicUsize::new(0),
        cpu: AtomicUsize::new(usize::MAX),
        when: AtomicU64::new(0),
    }) else {
        return Some(false);
    };
    let Ok(event) = DeadlineEvent::try_new(owner, result.clone()) else {
        return Some(false);
    };
    let worker_event = event.clone();
    let worker_result = result.clone();
    let Ok(closure) = Box::try_new(move || {
        let ok = exercise(&worker_event, &worker_result, producer);
        worker_result.worker_done.complete();
        if ok {
            0
        } else {
            1
        }
    }) else {
        return Some(false);
    };
    let Some(worker) = KernelThreadMechanism::create_on_cpu(
        KernelThreadClosure::EmptyClosure((closure, ())),
        "deadline-remote-selftest".to_string(),
        producer,
    ) else {
        return Some(false);
    };
    let completed = ProcessManager::wakeup(&worker).is_ok()
        && matches!(result.worker_done.wait_for_completion_timeout((5 * HZ) as i64), Ok(left) if left > 0);
    // Stop cannot release a private completion wait by itself. Wake that wait
    // first, join every created producer, then cancel its final possible arm.
    result.abort.store(true, Ordering::Release);
    result.callback_done.complete_all();
    let stopped = KernelThreadMechanism::stop(&worker);
    event.cancel();
    Some(completed && stopped == Ok(0) && result.calls.load(Ordering::Acquire) == 2)
}

#[cfg(feature = "mm_selftest")]
fn deadline_queue_selftest() {
    fn insert(queue: &mut Queue, deadline: u64, id: usize) {
        let mut node = Node::try_new().expect("deadline fixture allocation");
        node.key = (deadline, id);
        queue.pending.insert(node);
    }
    let mut queue = Queue::new();
    queue.next_tick = 100;
    assert_eq!(queue.next_deadline(), 100);
    insert(&mut queue, 200, 1);
    assert_eq!(queue.next_deadline(), 100);
    insert(&mut queue, 50, 2);
    assert_eq!(queue.next_deadline(), 50);
    let mut node = queue.pending.remove(&(50, 2)).unwrap();
    node.key = (150, 2);
    queue.pending.insert(node);
    assert_eq!(queue.next_deadline(), 100);
    queue.pending.remove(&(150, 2)).unwrap();
    insert(&mut queue, 50, 2);
    insert(&mut queue, 50, 3);
    queue.pending.remove(&(50, 2)).unwrap();
    assert_eq!(queue.next_deadline(), 50);
    queue.pending.remove(&(50, 3)).unwrap();
    assert_eq!(queue.next_deadline(), 100);
    insert(&mut queue, 100, 4);
    assert_eq!(queue.next_deadline(), 100);
    queue.pending.remove(&(100, 4)).unwrap();
    assert_eq!(queue.next_deadline(), 100);
    queue.pending.remove(&(200, 1)).unwrap();
    assert_eq!(queue.next_deadline(), 100);
}

/// Reuse the prepared event: this IRQ-off interval performs no allocation.
/// Remote producers may change pending work, but cannot change our next_tick or
/// execute this CPU's programming/IPI path until IRQs are restored. Both far
/// deadlines remain after next_tick regardless of any remote pending events.
#[cfg(feature = "mm_selftest")]
fn nonhead_reprogram_selftest(event: &Arc<DeadlineEvent>) {
    assert!(!CurrentIrqArch::is_irq_enabled());
    let cpu = smp_get_processor_id();
    assert_eq!(event.owner, cpu);
    let next_tick = QUEUES[cpu.data() as usize].lock_irqsave().next_tick;
    assert!(next_tick < u64::MAX - 2);
    let attempts = &PROGRAM_ATTEMPTS[cpu.data() as usize];
    let before = attempts.load(Ordering::Relaxed);
    event.arm(u64::MAX - 2).unwrap();
    event.arm(u64::MAX - 1).unwrap();
    event.cancel();
    assert_eq!(
        attempts.load(Ordering::Relaxed) - before,
        0,
        "nonhead deadline mutations must not reprogram clockevent"
    );
    let before = attempts.load(Ordering::Relaxed);
    reprogram_local();
    assert_eq!(attempts.load(Ordering::Relaxed) - before, 1);
}

fn state_spare_identity(event: &DeadlineEvent) -> usize {
    event
        .state
        .lock_irqsave()
        .spare
        .as_deref()
        .map_or(0, |node| node as *const Node as usize)
}

/// Exercise real interrupt delivery after a local clockevent downgrade. This
/// debug selftest must run from the serialized kthread selftest entry with IRQs
/// enabled. It changes neither the global clocksource nor other CPUs' modes.
pub fn run_fallback_selftest() -> bool {
    use crate::{arch::CurrentTimeArch, process::preempt::PreemptGuard, time::TimeArch};
    use core::{hint::spin_loop, sync::atomic::AtomicU64};

    if !CurrentIrqArch::is_irq_enabled() {
        return false;
    }
    // Pin the capability override to one CPU, but keep interrupts enabled so
    // the ordinary tick and the actual deadline IRQ can make progress.
    let _preempt = PreemptGuard::new();
    if !supported() {
        return false;
    }

    #[derive(Debug)]
    struct FallbackProbe {
        calls: AtomicU64,
        delivered_in_fallback: AtomicBool,
    }
    impl DeadlineCallback for FallbackProbe {
        fn expire(&self, _now: u64) -> Option<u64> {
            self.delivered_in_fallback
                .store(!supported(), Ordering::Release);
            self.calls.fetch_add(1, Ordering::Release);
            None
        }
    }
    struct Restore {
        cpu: ProcessorId,
        capable: bool,
        event: Arc<DeadlineEvent>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            let _irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
            self.event.cancel();
            CAPABLE[self.cpu.data() as usize].store(self.capable, Ordering::Release);
            reprogram_local();
        }
    }

    let cpu = smp_get_processor_id();
    let probe = Arc::new(FallbackProbe {
        calls: AtomicU64::new(0),
        delivered_in_fallback: AtomicBool::new(false),
    });
    let event = DeadlineEvent::new(cpu, probe.clone());
    let _restore = Restore {
        cpu,
        capable: CAPABLE[cpu.data() as usize].load(Ordering::Acquire),
        event: event.clone(),
    };
    let initial_jiffies;
    {
        let _irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
        if event.arm(now_ns().saturating_add(2 * TICK_NS)).is_err() {
            return false;
        }
        initial_jiffies = super::timer::clock();
        CAPABLE[cpu.data() as usize].store(false, Ordering::Release);
        reprogram_local();
    }

    // The timeout does not depend on the clockevent being tested. An iteration
    // cap also bounds failure if the architecture cycle counter stops.
    let start = CurrentTimeArch::get_cycles();
    let cycle_budget = CurrentTimeArch::cal_expire_cycles(100_000_000).wrapping_sub(start);
    for _ in 0..10_000_000 {
        if probe.calls.load(Ordering::Acquire) != 0
            && super::timer::clock().wrapping_sub(initial_jiffies) != 0
        {
            return probe.delivered_in_fallback.load(Ordering::Acquire);
        }
        if CurrentTimeArch::get_cycles().wrapping_sub(start) >= cycle_budget {
            break;
        }
        spin_loop();
    }
    false
}
