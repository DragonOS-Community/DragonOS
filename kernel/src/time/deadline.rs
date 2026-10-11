//! Fixed-owner monotonic deadline events, sharing the architecture clockevent
//! with the ordinary scheduler tick. This is not a userspace hrtimer API.
use alloc::{collections::BTreeMap, sync::Arc};
use core::{
    fmt::Debug,
    sync::atomic::{AtomicBool, Ordering},
};
use system_error::SystemError;

use super::{clocksource::HZ, tick_common::tick_handle_periodic, timekeeping};
use crate::{
    arch::{interrupt::TrapFrame, CurrentIrqArch},
    exception::InterruptArch,
    libs::spinlock::SpinLock,
    mm::percpu::PerCpu,
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
};

const TICK_NS: u64 = 1_000_000_000 / HZ;
const MAX_CALLBACKS: usize = 32;
static CONTINUOUS: AtomicBool = AtomicBool::new(false);
static CAPABLE: [AtomicBool; PerCpu::MAX_CPU_NUM as usize] =
    [const { AtomicBool::new(false) }; PerCpu::MAX_CPU_NUM as usize];
static QUEUES: [SpinLock<Queue>; PerCpu::MAX_CPU_NUM as usize] =
    [const { SpinLock::new(Queue::new()) }; PerCpu::MAX_CPU_NUM as usize];

type Key = (u64, usize);
struct Pending {
    event: Arc<DeadlineEvent>,
    generation: u64,
}
struct Queue {
    ready: bool,
    next_tick: u64,
    pending: BTreeMap<Key, Pending>,
}
impl Queue {
    const fn new() -> Self {
        Self {
            ready: false,
            next_tick: 0,
            pending: BTreeMap::new(),
        }
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
}
#[derive(Debug)]
pub struct DeadlineEvent {
    owner: ProcessorId,
    callback: Arc<dyn DeadlineCallback>,
    state: SpinLock<State>,
}

impl DeadlineEvent {
    pub fn new(owner: ProcessorId, callback: Arc<dyn DeadlineCallback>) -> Arc<Self> {
        Arc::new(Self {
            owner,
            callback,
            state: SpinLock::new(State {
                generation: 0,
                queued: None,
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
        let mut state = self.state.lock_irqsave();
        let mut queue = QUEUES[self.owner.data() as usize].lock_irqsave();
        if !queue.ready {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if let Some(key) = state.queued.take() {
            queue.pending.remove(&key);
        }
        state.generation = state.generation.wrapping_add(1);
        let key = (deadline_ns, Arc::as_ptr(self) as usize);
        queue.pending.insert(
            key,
            Pending {
                event: self.clone(),
                generation: state.generation,
            },
        );
        state.queued = Some(key);
        if self.owner == smp_get_processor_id() {
            program_locked(&queue);
        }
        drop(queue);
        drop(state);
        kick_owner(self.owner);
        Ok(())
    }

    /// Cancels queued/rearm work; a callback already entered may finish.
    pub fn cancel(&self) {
        let mut state = self.state.lock_irqsave();
        state.generation = state.generation.wrapping_add(1);
        let Some(key) = state.queued.take() else {
            return;
        };
        let mut queue = QUEUES[self.owner.data() as usize].lock_irqsave();
        queue.pending.remove(&key);
        if self.owner == smp_get_processor_id() {
            program_locked(&queue);
        }
        drop(queue);
        drop(state);
        kick_owner(self.owner);
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
    if !supported() {
        #[cfg(target_arch = "x86_64")]
        crate::arch::driver::apic::apic_timer::restore_periodic();
        #[cfg(target_arch = "riscv64")]
        crate::driver::clocksource::timer_riscv::program_deadline_delta(TICK_NS);
        return;
    }
    let next = queue
        .pending
        .first_key_value()
        .map_or(queue.next_tick, |(key, _)| key.0.min(queue.next_tick));
    let delta = next.saturating_sub(now_ns()).max(1);
    #[cfg(target_arch = "x86_64")]
    crate::arch::driver::apic::apic_timer::program_deadline_delta(delta);
    #[cfg(target_arch = "riscv64")]
    crate::driver::clocksource::timer_riscv::program_deadline_delta(delta);
}

fn kick_owner(owner: ProcessorId) {
    if owner == smp_get_processor_id() {
        return;
    }
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
            if queue
                .pending
                .first_key_value()
                .is_none_or(|(key, _)| key.0 > now)
            {
                None
            } else {
                queue.pending.pop_first()
            }
        };
        let Some((key, pending)) = pending else {
            break;
        };
        dispatch_pending(key, pending, now);
    }
    reprogram_local();
}

fn dispatch_pending(key: Key, pending: Pending, now: u64) {
    let event = pending.event;
    {
        let mut state = event.state.lock_irqsave();
        if state.generation != pending.generation || state.queued != Some(key) {
            return;
        }
        state.queued = None;
    }
    finish_callback(&event, pending.generation, now);
}

fn finish_callback(event: &Arc<DeadlineEvent>, generation: u64, now: u64) {
    if let Some(deadline) = event.callback.expire(now) {
        let mut state = event.state.lock_irqsave();
        if state.generation != generation {
            return;
        }
        let key = (deadline, Arc::as_ptr(event) as usize);
        let mut queue = QUEUES[event.owner.data() as usize].lock_irqsave();
        queue.pending.insert(
            key,
            Pending {
                event: event.clone(),
                generation,
            },
        );
        state.queued = Some(key);
    }
}

/// Production generation and queue primitives, without changing interrupt
/// delivery or relying on a userspace test syscall.
pub fn run_selftests() {
    use alloc::sync::Weak;
    use core::sync::atomic::AtomicU64;
    #[derive(Debug)]
    struct Probe {
        calls: AtomicU64,
        cancel: AtomicBool,
        event: SpinLock<Weak<DeadlineEvent>>,
    }
    impl DeadlineCallback for Probe {
        fn expire(&self, now: u64) -> Option<u64> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.cancel.load(Ordering::Relaxed) {
                let event = self.event.lock_irqsave().upgrade();
                event.unwrap().cancel();
            }
            Some(now.saturating_add(TICK_NS))
        }
    }
    let irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
    if supported() {
        let probe = Arc::new(Probe {
            calls: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            event: SpinLock::new(Weak::new()),
        });
        let event = DeadlineEvent::new(smp_get_processor_id(), probe.clone());
        *probe.event.lock_irqsave() = Arc::downgrade(&event);
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
            .unwrap();
        dispatch_pending(key, popped, deadline);
        assert_eq!(probe.calls.load(Ordering::Relaxed), 1);
        assert!(
            event.state.lock_irqsave().queued.is_some(),
            "callback deadline must rearm"
        );
        event.cancel();

        probe.cancel.store(true, Ordering::Relaxed);
        event.arm(deadline).unwrap();
        let key = event.state.lock_irqsave().queued.unwrap();
        let popped = QUEUES[event.owner.data() as usize]
            .lock_irqsave()
            .pending
            .remove(&key)
            .unwrap();
        dispatch_pending(key, popped, deadline);
        assert_eq!(probe.calls.load(Ordering::Relaxed), 2);
        assert!(
            event.state.lock_irqsave().queued.is_none(),
            "callback cancel must prevent old rearm"
        );
        assert!(!QUEUES[event.owner.data() as usize]
            .lock_irqsave()
            .pending
            .values()
            .any(|p| Arc::ptr_eq(&p.event, &event)));
        reprogram_local();
    }
    drop(irq);
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
