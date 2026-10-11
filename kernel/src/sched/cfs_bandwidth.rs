//! Shared CFS bandwidth. Runtime belongs to a task group, not to each CPU.
//! Lock order is rq -> budget. Deadline callbacks release budget before rq.
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, Ordering};
use system_error::SystemError;

use super::fair::FairSchedEntity;
use super::{cpu_is_online, cpu_rq, CpuRunQueue, SchedClass, TaskGroup};
use crate::process::ProcessControlBlock;
use crate::{
    libs::spinlock::SpinLock,
    mm::percpu::PerCpu,
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
    time::deadline::{self, DeadlineCallback, DeadlineEvent},
};

const SLICE_NS: i64 = 5_000_000;
const RETAIN_NS: i64 = 1_000_000;
const SLACK_NS: u64 = 5_000_000;
const MIN_PERIOD_REMAIN_NS: u64 = 2_000_000;
const CPUS: usize = PerCpu::MAX_CPU_NUM as usize;
static RUNTIME_EVENTS: [SpinLock<Option<Arc<DeadlineEvent>>>; CPUS] =
    [const { SpinLock::new(None) }; CPUS];

#[derive(Debug)]
struct RuntimeDeadline {
    cpu: ProcessorId,
}
impl DeadlineCallback for RuntimeDeadline {
    fn expire(&self, now: u64) -> Option<u64> {
        let binding = cpu_rq(self.cpu.data() as usize);
        let (rq, _guard) = binding.self_lock();
        rq.update_rq_clock();
        let current = rq.current();
        if current.sched_info().sched_class() != SchedClass::Fair {
            return None;
        }
        let mut se = current.sched_info().sched_entity();
        FairSchedEntity::for_each_in_group(&mut se, |se| {
            se.cfs_rq().force_mut().update_current();
            (true, true)
        });
        match remaining_runtime(&current) {
            Some(remaining) if remaining > 0 => Some(now.saturating_add(remaining as u64)),
            Some(_) => {
                rq.resched_current();
                None
            }
            None => None,
        }
    }
}

fn remaining_runtime(task: &Arc<ProcessControlBlock>) -> Option<i64> {
    if task.sched_info().sched_class() != SchedClass::Fair {
        return None;
    }
    let mut remaining: Option<i64> = None;
    let mut se = task.sched_info().sched_entity();
    FairSchedEntity::for_each_in_group(&mut se, |se| {
        let cfs = se.cfs_rq();
        if cfs.runtime_enabled {
            remaining =
                Some(remaining.map_or(cfs.runtime_remaining, |r| r.min(cfs.runtime_remaining)));
        }
        (true, true)
    });
    remaining
}

/// Exactly one exhaustion event per CPU, not one timer for every runnable
/// task. Caller holds rq and passes the next task when rq.current is still old.
pub(crate) fn refresh_runtime_event(rq: &mut CpuRunQueue, task: &Arc<ProcessControlBlock>) {
    let remaining = remaining_runtime(task);
    let mut slot = RUNTIME_EVENTS[rq.cpu().data() as usize].lock_irqsave();
    if remaining.is_some() && slot.is_none() {
        *slot = Some(DeadlineEvent::new(
            rq.cpu(),
            Arc::new(RuntimeDeadline { cpu: rq.cpu() }),
        ));
    }
    let Some(event) = slot.as_ref() else {
        return;
    };
    match remaining {
        Some(remaining) if remaining > 0 => {
            let _ = event.arm(deadline::now_ns().saturating_add(remaining as u64));
        }
        Some(_) => {
            event.cancel();
            rq.resched_current();
        }
        None => event.cancel(),
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct BandwidthSnapshot {
    pub(crate) nr_periods: u64,
    pub(crate) nr_throttled: u64,
    pub(crate) throttled_ns: u64,
    pub(crate) nr_bursts: u64,
    pub(crate) burst_ns: u64,
    pub(crate) local_throttled_ns: u64,
}

#[derive(Debug)]
struct Budget {
    quota: Option<u64>,
    period: u64,
    burst: u64,
    available: u64,
    period_start_available: u64,
    next_period: u64,
    generation: u64,
    active: bool,
    idle: bool,
    slack_started: bool,
    throttled: [bool; CPUS],
    throttle_start: [Option<u64>; CPUS],
    local_start: [Option<u64>; CPUS],
    stats: BandwidthSnapshot,
}

impl Budget {
    fn replenish(&mut self) {
        let Some(quota) = self.quota else {
            return;
        };
        let replenished = self.available.saturating_add(quota);
        let burst_used = self.period_start_available.saturating_sub(replenished);
        if burst_used != 0 {
            self.stats.nr_bursts = self.stats.nr_bursts.saturating_add(1);
            self.stats.burst_ns = self.stats.burst_ns.saturating_add(burst_used);
        }
        self.available = replenished.min(quota.saturating_add(self.burst));
        self.period_start_available = self.available;
    }
}

#[derive(Debug)]
pub(crate) struct CfsBandwidth {
    group: Weak<TaskGroup>,
    budget: SpinLock<Budget>,
    period_event: Arc<DeadlineEvent>,
    slack_event: Arc<DeadlineEvent>,
    online: AtomicBool,
}

#[derive(Debug)]
struct Refill {
    group: Weak<TaskGroup>,
    slack: bool,
}
impl DeadlineCallback for Refill {
    fn expire(&self, now: u64) -> Option<u64> {
        let group = self.group.upgrade()?;
        group.bandwidth.refill(now, self.slack)
    }
}

impl CfsBandwidth {
    pub(crate) fn new(group: Weak<TaskGroup>) -> Arc<Self> {
        let owner = smp_get_processor_id();
        Arc::new(Self {
            period_event: DeadlineEvent::new(
                owner,
                Arc::new(Refill {
                    group: group.clone(),
                    slack: false,
                }),
            ),
            slack_event: DeadlineEvent::new(
                owner,
                Arc::new(Refill {
                    group: group.clone(),
                    slack: true,
                }),
            ),
            group,
            budget: SpinLock::new(Budget {
                quota: None,
                period: 100_000_000,
                burst: 0,
                available: 0,
                period_start_available: 0,
                next_period: 0,
                generation: 0,
                active: false,
                idle: true,
                slack_started: false,
                throttled: [false; CPUS],
                throttle_start: [None; CPUS],
                local_start: [None; CPUS],
                stats: BandwidthSnapshot::default(),
            }),
            online: AtomicBool::new(true),
        })
    }

    /// Called under the owner rq. Negative local credit is carried, never
    /// forgiven by migration, a delayed tick, or a period boundary.
    pub(crate) fn assign(&self, remaining: &mut i64, target: i64) -> bool {
        let mut b = self.budget.lock_irqsave();
        if b.quota.is_none() {
            return true;
        }
        if !self.online.load(Ordering::Acquire) {
            return false;
        }
        if !b.active {
            b.active = true;
            b.next_period = deadline::now_ns().saturating_add(b.period);
            // Keep the budget locked until the event has been queued: a
            // concurrent configuration must not cancel before this arm.
            if self.period_event.arm(b.next_period).is_err() {
                // A clocksource downgrade does not strand existing queues:
                // already armed events are serviced by the periodic fallback.
                b.active = false;
            }
        }
        let needed = target.saturating_sub(*remaining).max(0) as u64;
        let grant = needed.min(b.available);
        b.available -= grant;
        *remaining = remaining.saturating_add(grant as i64);
        b.idle = false;
        *remaining > 0
    }

    pub(crate) fn assign_slice(&self, remaining: &mut i64) -> bool {
        self.assign(remaining, SLICE_NS)
    }

    /// The last-credit test and wait-list publication are one budget critical
    /// section. Otherwise a refill between them can miss this runqueue.
    pub(crate) fn try_throttle(&self, cpu: ProcessorId, remaining: &mut i64, now: u64) -> bool {
        let mut b = self.budget.lock_irqsave();
        if b.quota.is_none() {
            return false;
        }
        let needed = 1i64.saturating_sub(*remaining).max(0) as u64;
        let grant = needed.min(b.available);
        b.available -= grant;
        *remaining = remaining.saturating_add(grant as i64);
        if *remaining > 0 {
            b.idle = false;
            return false;
        }
        let index = cpu.data() as usize;
        b.throttled[index] = true;
        b.throttle_start[index].get_or_insert(now);
        true
    }

    pub(crate) fn mark_unthrottled(&self, cpu: ProcessorId, now: u64) {
        let mut b = self.budget.lock_irqsave();
        let index = cpu.data() as usize;
        b.throttled[index] = false;
        if let Some(start) = b.throttle_start[index].take() {
            b.stats.throttled_ns = b
                .stats
                .throttled_ns
                .saturating_add(now.saturating_sub(start));
        }
    }

    /// Local queue time includes ancestor throttling, just like Linux
    /// throttled_clock_self_time. Call on hierarchy 0->1 and 1->0 transitions.
    pub(crate) fn freeze_local(&self, cpu: ProcessorId, now: u64) {
        self.budget.lock_irqsave().local_start[cpu.data() as usize].get_or_insert(now);
    }

    pub(crate) fn thaw_local(&self, cpu: ProcessorId, now: u64) {
        let mut b = self.budget.lock_irqsave();
        if let Some(start) = b.local_start[cpu.data() as usize].take() {
            b.stats.local_throttled_ns = b
                .stats
                .local_throttled_ns
                .saturating_add(now.saturating_sub(start));
        }
    }

    pub(crate) fn return_slack(&self, remaining: &mut i64) {
        if *remaining <= RETAIN_NS {
            return;
        }
        let mut b = self.budget.lock_irqsave();
        if b.quota.is_none() {
            return;
        }
        let returned = (*remaining - RETAIN_NS) as u64;
        *remaining = RETAIN_NS;
        b.available = b.available.saturating_add(returned);
        let now = deadline::now_ns();
        if !b.slack_started
            && b.throttled.iter().any(|x| *x)
            && b.next_period.saturating_sub(now) > SLACK_NS + MIN_PERIOD_REMAIN_NS
        {
            b.slack_started = self.slack_event.arm(now.saturating_add(SLACK_NS)).is_ok();
        }
    }

    fn refill(&self, now: u64, slack: bool) -> Option<u64> {
        let (generation, targets, next) = {
            let mut b = self.budget.lock_irqsave();
            if slack {
                b.slack_started = false;
            }
            if !self.online.load(Ordering::Acquire) || b.quota.is_none() {
                return None;
            }
            if !slack {
                if now < b.next_period {
                    return Some(b.next_period);
                }
                let overrun = (now - b.next_period) / b.period + 1;
                b.stats.nr_periods = b.stats.nr_periods.saturating_add(overrun);
                if b.throttled.iter().any(|x| *x) {
                    b.stats.nr_throttled = b.stats.nr_throttled.saturating_add(overrun);
                }
                // Linux refills once, not overrun times. Replaying missed
                // periods would create a catch-up burst after interrupt delay.
                b.replenish();
                b.next_period = b
                    .next_period
                    .saturating_add(overrun.saturating_mul(b.period));
                if b.idle && !b.throttled.iter().any(|x| *x) {
                    b.active = false;
                    return None;
                }
                b.idle = true;
            } else if b.next_period.saturating_sub(now) <= MIN_PERIOD_REMAIN_NS {
                return None;
            }
            (b.generation, b.throttled, b.next_period)
        };
        let group = self.group.upgrade()?;
        for (cpu, target) in targets.into_iter().enumerate() {
            if !target {
                continue;
            }
            let binding = cpu_rq(cpu);
            let (rq, _guard) = binding.self_lock();
            rq.update_rq_clock();
            // Revalidate after rq acquisition; never budget -> rq nesting.
            if self.budget.lock_irqsave().generation != generation {
                continue;
            }
            let leaf = group.cfs_rq(ProcessorId::new(cpu as u32));
            let leaf = leaf.force_mut();
            if leaf.throttled && self.assign(&mut leaf.runtime_remaining, 1) {
                leaf.unthrottle_bandwidth();
                rq.resched_current();
            }
        }
        if slack {
            None
        } else {
            Some(next)
        }
    }

    pub(crate) fn configure(
        &self,
        quota_us: Option<u64>,
        period_us: u64,
        burst_us: u64,
    ) -> Result<(), SystemError> {
        if quota_us.is_some() && !deadline::all_online_supported() {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        {
            let mut b = self.budget.lock_irqsave();
            b.generation = b.generation.wrapping_add(1);
            self.period_event.cancel();
            self.slack_event.cancel();
            b.slack_started = false;
            b.quota = quota_us.map(|v| v * 1000);
            b.period = period_us * 1000;
            b.burst = burst_us * 1000;
            b.replenish();
            b.active = quota_us.is_some();
            b.idle = true;
            if b.active {
                b.next_period = deadline::now_ns().saturating_add(b.period);
                self.period_event.arm(b.next_period)?;
            }
        }
        if let Some(group) = self.group.upgrade() {
            for cpu in 0..CPUS {
                let binding = cpu_rq(cpu);
                let (rq, _guard) = binding.self_lock();
                rq.update_rq_clock();
                let leaf = group.cfs_rq(ProcessorId::new(cpu as u32));
                let leaf = leaf.force_mut();
                leaf.runtime_enabled = quota_us.is_some();
                leaf.runtime_remaining = 0;
                if leaf.throttled {
                    leaf.unthrottle_bandwidth();
                }
                if cpu_is_online(ProcessorId::new(cpu as u32)) {
                    rq.resched_current();
                }
            }
        }
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> BandwidthSnapshot {
        let b = self.budget.lock_irqsave();
        let now = deadline::now_ns();
        let mut stats = b.stats;
        for start in b.local_start.iter().flatten() {
            stats.local_throttled_ns = stats
                .local_throttled_ns
                .saturating_add(now.saturating_sub(*start));
        }
        stats
    }

    pub(crate) fn retire(&self) {
        self.online.store(false, Ordering::Release);
        let mut b = self.budget.lock_irqsave();
        b.generation = b.generation.wrapping_add(1);
        self.period_event.cancel();
        self.slack_event.cancel();
        b.slack_started = false;
    }
}

impl TaskGroup {
    pub(crate) fn configure_bandwidth(
        &self,
        quota: Option<u64>,
        period: u64,
        burst: u64,
    ) -> Result<(), SystemError> {
        self.bandwidth.configure(quota, period, burst)
    }
    pub(crate) fn bandwidth_snapshot(&self) -> BandwidthSnapshot {
        self.bandwidth.snapshot()
    }
}
