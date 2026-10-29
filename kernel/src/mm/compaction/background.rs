//! Failed-request recovery and bounded replenishment for recent real demand.
//! No allocation promise or page ownership is retained for the original device.
use alloc::{string::ToString, sync::Arc};
use core::sync::atomic::{AtomicU64, Ordering};
use system_error::SystemError;
use unified_init::macros::unified_init;

use super::{
    recovery::{Service, ServiceError},
    CompactedFrames, Stop, MAX_COMPACT_ORDER,
};
use crate::{
    arch::mm::LockedFrameAllocator,
    init::initcall::INITCALL_CORE,
    libs::{
        mutex::Mutex,
        spinlock::SpinLock,
        wait_queue::{Waiter, Waker},
    },
    mm::{
        allocator::{
            buddy::{AllocationFailure, CompactionSnapshot, DmaAllocationFailure, DmaRecoveryZone},
            page_frame::PageFrameCount,
            reserve::ReserveLevel,
            watermark::ZonePressureSnapshot,
        },
        page::PageReclaimer,
        PhysAddr,
    },
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock, ProcessManager,
    },
    time::timer::{clock, next_n_us_timer_jiffies},
};

const SLOTS: usize = 2 * MAX_COMPACT_ORDER;
const SLICE_US: u64 = 10_000;
const MIN_INTERVAL_US: u64 = 10_000;
const MAX_INTERVAL_US: u64 = 1_000_000;

#[derive(Clone, Copy)]
struct Slot {
    // Only unconsumed endpoints live here. Active work never rejoins a merge.
    pending: Option<(usize, usize)>,
    wide_next: bool,
    active: bool,
    failures: u8,
    not_before: u64,
}
impl Slot {
    const fn new() -> Self {
        Self {
            pending: None,
            wide_next: false,
            active: false,
            failures: 0,
            not_before: 0,
        }
    }
}
#[derive(Clone, Copy)]
struct Work {
    slot: usize,
    max: PhysAddr,
}
impl Work {
    fn count(self) -> PageFrameCount {
        PageFrameCount::new(1 << (self.slot % MAX_COMPACT_ORDER + 1))
    }
    fn order(self) -> usize {
        self.slot % MAX_COMPACT_ORDER + 1
    }
}
struct Pending {
    slots: [Slot; SLOTS],
    next: usize,
    proactive: Proactive,
    prefer_proactive: bool,
}
impl Pending {
    const fn new() -> Self {
        Self {
            slots: [Slot::new(); SLOTS],
            next: 0,
            proactive: Proactive::new(),
            prefer_proactive: false,
        }
    }

    fn post(&mut self, slot: usize, max: usize) -> bool {
        let entry = &mut self.slots[slot];
        let notify = entry.pending.is_none() && !entry.active;
        entry.pending = Some(match entry.pending {
            Some((low, high)) => (low.min(max), high.max(max)),
            None => (max, max),
        });
        notify
    }

    fn take_ready(&mut self, now: u64) -> Option<Work> {
        for offset in 0..SLOTS {
            let index = (self.next + offset) % SLOTS;
            let entry = &mut self.slots[index];
            if entry.active || entry.not_before > now {
                continue;
            }
            let Some((low, high)) = entry.pending else {
                continue;
            };
            let max = if entry.wide_next { high } else { low };
            entry.pending = if low == high {
                None
            } else {
                let remaining = if entry.wide_next { low } else { high };
                Some((remaining, remaining))
            };
            entry.wide_next = !entry.wide_next;
            entry.active = true;
            self.next = (index + 1) % SLOTS;
            return Some(Work {
                slot: index,
                max: PhysAddr::new(max),
            });
        }
        None
    }

    fn finish(&mut self, work: Work, outcome: &Result<(), ServiceError>) {
        let slot = &mut self.slots[work.slot];
        // Even successful and advancing discovery rounds retain the minimum
        // interval. No result republishes a request or clears real failures
        // merely because a partial registry window had no eligible source.
        let delay = match outcome {
            Ok(()) => {
                slot.failures = 0;
                MIN_INTERVAL_US
            }
            Err(ServiceError::DiscoveryProgress) => MIN_INTERVAL_US,
            Err(ServiceError::Stopped(_)) => {
                slot.failures = slot.failures.saturating_add(1).min(8);
                (MIN_INTERVAL_US << slot.failures.saturating_sub(1)).min(MAX_INTERVAL_US)
            }
        };
        slot.not_before = next_n_us_timer_jiffies(delay);
        slot.active = false;
    }

    fn deadline(&self) -> u64 {
        self.slots
            .iter()
            .filter(|slot| !slot.active && slot.pending.is_some())
            .map(|slot| slot.not_before.max(1))
            .min()
            .unwrap_or(0)
    }
}

const LEASE_US: u64 = 10_000_000;
const CHECK_US: u64 = 500_000;
const DISCOVERY_US: u64 = 10_000;
const PROACTIVE_MAX_BACKOFF_US: u64 = 32_000_000;
const LOW_RUNS: usize = 1;
const HIGH_RUNS: usize = 2;

fn after(now: u64, micros: u64) -> u64 {
    // Match timer's positive-duration rounding, including the current tick.
    now.saturating_add(
        micros
            .saturating_mul(crate::time::clocksource::HZ)
            .div_ceil(1_000_000),
    )
    .saturating_add(1)
}

#[derive(Clone, Copy)]
struct Demand {
    max: usize,
    expires: u64,
    check_after: u64,
    refilling: bool,
}
#[derive(Clone, Copy)]
struct DemandSlot {
    endpoints: [Option<Demand>; 2],
    next: usize,
    active: bool,
    failures: u8,
    not_before: u64,
}
impl DemandSlot {
    const fn new() -> Self {
        Self {
            endpoints: [None; 2],
            next: 0,
            active: false,
            failures: 0,
            not_before: 0,
        }
    }
}
struct Proactive {
    slots: [DemandSlot; SLOTS],
    next: usize,
    slice_after: u64,
    discovery_after: u64,
    expired: u64,
}
#[derive(Clone, Copy)]
struct ProactiveWork {
    work: Work,
    endpoint: usize,
    expires: u64,
    refilling: bool,
    slice_after: u64,
}
#[derive(Clone, Copy)]
enum ProactiveState {
    Healthy,
    Cooling(u64),
    Discovery,
    Progress { refilling: bool },
    Failed(Stop),
    Expired,
}
struct ProactiveResult {
    state: ProactiveState,
    used_slice: bool,
}
#[derive(Clone, Copy)]
enum SelectedWork {
    Urgent(Work),
    Proactive(ProactiveWork),
}
impl Proactive {
    const fn new() -> Self {
        Self {
            slots: [DemandSlot::new(); SLOTS],
            next: 0,
            slice_after: 0,
            discovery_after: 0,
            expired: 0,
        }
    }
    fn expire(&mut self, now: u64) {
        for slot in &mut self.slots {
            for endpoint in &mut slot.endpoints {
                if endpoint.is_some_and(|demand| demand.expires <= now) {
                    *endpoint = None;
                    self.expired = self.expired.saturating_add(1);
                }
            }
        }
    }
    fn post(&mut self, index: usize, max: usize, now: u64) {
        self.expire(now);
        let slot = &mut self.slots[index];
        if let Some(endpoint) = slot
            .endpoints
            .iter_mut()
            .flatten()
            .find(|entry| entry.max == max)
        {
            endpoint.expires = after(now, LEASE_US);
            return;
        }
        let selected = slot
            .endpoints
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| {
                usize::from(slot.endpoints[1].unwrap().expires < slot.endpoints[0].unwrap().expires)
            });
        slot.endpoints[selected] = Some(Demand {
            max,
            expires: after(now, LEASE_US),
            check_after: after(now, CHECK_US),
            refilling: false,
        });
        // New demand never clears shared failure history or either global gate.
    }
    fn take_ready(&mut self, now: u64) -> Option<ProactiveWork> {
        self.expire(now);
        if now < self.discovery_after {
            return None;
        }
        for offset in 0..SLOTS {
            let index = (self.next + offset) % SLOTS;
            let slot = &mut self.slots[index];
            if slot.active || now < slot.not_before {
                continue;
            }
            for step in 0..2 {
                let endpoint = (slot.next + step) % 2;
                let Some(demand) = slot.endpoints[endpoint] else {
                    continue;
                };
                if now < demand.check_after {
                    continue;
                }
                slot.active = true;
                slot.next = (endpoint + 1) % 2;
                self.next = (index + 1) % SLOTS;
                self.discovery_after = after(now, DISCOVERY_US);
                return Some(ProactiveWork {
                    work: Work {
                        slot: index,
                        max: PhysAddr::new(demand.max),
                    },
                    endpoint,
                    expires: demand.expires,
                    refilling: demand.refilling,
                    slice_after: self.slice_after,
                });
            }
        }
        None
    }
    fn finish(&mut self, work: ProactiveWork, result: &ProactiveResult, now: u64) {
        self.discovery_after = after(now, DISCOVERY_US);
        if result.used_slice {
            self.slice_after = after(now, CHECK_US);
        }
        let slot = &mut self.slots[work.work.slot];
        slot.active = false;
        let delay = match result.state {
            ProactiveState::Progress { .. } => {
                slot.failures = 0;
                CHECK_US
            }
            ProactiveState::Discovery => DISCOVERY_US,
            ProactiveState::Failed(_) => {
                slot.failures = slot.failures.saturating_add(1).min(7);
                (CHECK_US << slot.failures.saturating_sub(1)).min(PROACTIVE_MAX_BACKOFF_US)
            }
            _ => CHECK_US,
        };
        // Only increased usable inventory resets history. Healthy observations
        // and registry cursor progress cannot erase a genuine failed pass.
        if matches!(result.state, ProactiveState::Failed(_)) {
            slot.not_before = after(now, delay);
        }
        if let Some(endpoint) = &mut slot.endpoints[work.endpoint] {
            if endpoint.max == work.work.max.data() {
                endpoint.check_after = match result.state {
                    ProactiveState::Cooling(until) => until.max(after(now, DISCOVERY_US)),
                    ProactiveState::Expired if endpoint.expires != work.expires => {
                        after(now, DISCOVERY_US)
                    }
                    _ => after(now, delay),
                };
                match result.state {
                    ProactiveState::Healthy => endpoint.refilling = false,
                    ProactiveState::Progress { refilling } => endpoint.refilling = refilling,
                    // A foreground success may have renewed this same mask
                    // while the worker still held the old lease deadline.
                    ProactiveState::Expired => {
                        if endpoint.expires == work.expires {
                            endpoint.expires = now;
                        }
                    }
                    _ => endpoint.refilling = true,
                }
            }
        }
        self.expire(now);
    }
    fn deadline(&self) -> u64 {
        self.slots
            .iter()
            .flat_map(|slot| {
                slot.endpoints.iter().flatten().map(move |demand| {
                    if slot.active {
                        demand.expires
                    } else {
                        demand.expires.min(
                            demand
                                .check_after
                                .max(slot.not_before)
                                .max(self.discovery_after),
                        )
                    }
                })
            })
            .map(|time| time.max(1))
            .min()
            .unwrap_or(0)
    }
}
impl Pending {
    fn take_next(&mut self, now: u64) -> Option<SelectedWork> {
        let result = if self.prefer_proactive {
            self.proactive
                .take_ready(now)
                .map(SelectedWork::Proactive)
                .or_else(|| self.take_ready(now).map(SelectedWork::Urgent))
        } else {
            self.take_ready(now)
                .map(SelectedWork::Urgent)
                .or_else(|| self.proactive.take_ready(now).map(SelectedWork::Proactive))
        };
        if let Some(work) = &result {
            self.prefer_proactive = matches!(work, SelectedWork::Urgent(_));
        }
        result
    }
}

static PENDING: SpinLock<Pending> = SpinLock::new(Pending::new());
// Permanent storage, not a large service array on the worker's kernel stack.
static SERVICES: Mutex<[Service; SLOTS]> = Mutex::new([const { Service::new() }; SLOTS]);
static NEXT_DEADLINE: AtomicU64 = AtomicU64::new(0);
static PROACTIVE_DEADLINE: AtomicU64 = AtomicU64::new(0);
static WORKER: SpinLock<Option<Arc<ProcessControlBlock>>> = SpinLock::new(None);
static WAKER: SpinLock<Option<Arc<Waker>>> = SpinLock::new(None);

/// Only the real DMA failure site calls this after its synchronous attempt.
/// The zone is a dispatch hint; execution always obtains fresh capacity proof.
pub(crate) fn request(order: usize, max: PhysAddr, zone: DmaRecoveryZone) {
    if order == 0 || order > MAX_COMPACT_ORDER || !super::recovery::context_allowed() {
        return;
    }
    let domain = usize::from(!matches!(zone, DmaRecoveryZone::Normal));
    let slot = domain * MAX_COMPACT_ORDER + order - 1;
    let notify = PENDING.lock_irqsave().post(slot, max.data());
    #[cfg(feature = "mm_selftest")]
    selftest::REQUESTS.fetch_add(1, Ordering::Relaxed);
    if notify {
        crate::mm::memory_notify::request_compaction();
    }
}

/// Observe only externally delivered Kernel-reserve allocations. The caller
/// has released Buddy; internal probes/captures must never renew this lease.
pub(crate) fn observe_allocation(count: PageFrameCount, max: PhysAddr, physical: PhysAddr) {
    let pages = count.data();
    if pages <= 1 || !pages.is_power_of_two() {
        return;
    }
    let order = pages.trailing_zeros() as usize;
    if order > MAX_COMPACT_ORDER {
        return;
    }
    // An IRQ borrows the interrupted PCB but owns its device allocation.
    // Task-local metadata remains internal even with IRQs/preemption disabled.
    if !crate::exception::in_interrupt() && ProcessManager::initialized() {
        let current = ProcessManager::current_pcb();
        let internal = {
            let worker = WORKER.lock_irqsave();
            worker
                .as_ref()
                .is_some_and(|task| Arc::ptr_eq(task, &current))
        };
        // WORKER permanently pins its PCB. Drop the temporary current pin only
        // after releasing WORKER, before acquiring any policy or Buddy lock.
        drop(current);
        if internal {
            #[cfg(feature = "mm_selftest")]
            selftest::INTERNAL_SUPPRESSED.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    let normal = physical.data() >= (1usize << 32);
    let max = if normal {
        max.data()
    } else {
        max.data().min(u32::MAX as usize)
    };
    if physical
        .data()
        .checked_add(count.bytes())
        .and_then(|end| end.checked_sub(1))
        .is_none_or(|end| end > max)
    {
        return;
    }
    let slot = usize::from(!normal) * MAX_COMPACT_ORDER + order - 1;
    let notify = {
        let mut pending = PENDING.lock_irqsave();
        pending.proactive.post(slot, max, clock());
        let deadline = pending.proactive.deadline();
        let old = PROACTIVE_DEADLINE.swap(deadline, Ordering::AcqRel);
        old == 0 || deadline < old
    };
    #[cfg(feature = "mm_selftest")]
    selftest::PRO_DEMANDS.fetch_add(1, Ordering::Relaxed);
    if notify {
        crate::mm::memory_notify::request_compaction();
    }
}

/// Timer IRQ only queues a deferred notification; it never consumes the slot.
pub(crate) fn tick(now: u64) {
    let deadline = NEXT_DEADLINE.load(Ordering::Acquire);
    let proactive = PROACTIVE_DEADLINE.load(Ordering::Acquire);
    if deadline != 0 && now >= deadline || proactive != 0 && now >= proactive {
        crate::mm::memory_notify::request_compaction();
    }
}

/// Memory-notifier task context only. The permanent pins never leave bootstrap.
pub(crate) fn notify() {
    let waker = WAKER.lock_irqsave().as_ref().cloned();
    if let Some(waker) = waker {
        waker.wake();
    }
}

#[unified_init(INITCALL_CORE)]
fn init_background_compaction() -> Result<(), SystemError> {
    let closure = KernelThreadClosure::StaticEmptyClosure((&(worker as fn() -> i32), ()));
    let task = KernelThreadMechanism::create_on_cpu(
        closure,
        "page_compact".to_string(),
        crate::smp::core::smp_get_processor_id(),
    )
    .expect("background compaction bootstrap failed");
    *WORKER.lock_irqsave() = Some(task.clone());
    ProcessManager::wakeup(&task).expect("background compaction bootstrap wake failed");
    Ok(())
}

fn worker() -> i32 {
    let (waiter, waker) =
        Waiter::try_new_pair().expect("background compaction waiter bootstrap failed");
    *WAKER.lock_irqsave() = Some(waker);
    #[cfg(feature = "mm_selftest")]
    selftest::check_worker_allocation();
    loop {
        let work = {
            let mut pending = PENDING.lock_irqsave();
            let work = pending.take_next(clock());
            NEXT_DEADLINE.store(
                if matches!(work, Some(SelectedWork::Urgent(_))) {
                    0
                } else {
                    pending.deadline()
                },
                Ordering::Release,
            );
            PROACTIVE_DEADLINE.store(
                if matches!(work, Some(SelectedWork::Proactive(_))) {
                    0
                } else {
                    pending.proactive.deadline()
                },
                Ordering::Release,
            );
            work
        };
        if let Some(work) = work {
            match work {
                SelectedWork::Urgent(work) => {
                    #[cfg(feature = "mm_selftest")]
                    selftest::ROUNDS.fetch_add(1, Ordering::Relaxed);
                    let outcome = run(work);
                    #[cfg(feature = "mm_selftest")]
                    {
                        selftest::record_stop(&outcome);
                        if outcome.is_ok() {
                            selftest::SUCCESSES.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    PENDING.lock_irqsave().finish(work, &outcome);
                }
                SelectedWork::Proactive(work) => {
                    #[cfg(feature = "mm_selftest")]
                    selftest::record_proactive_start(work.work.slot);
                    let result = run_proactive(work);
                    #[cfg(feature = "mm_selftest")]
                    selftest::record_proactive(&result);
                    PENDING
                        .lock_irqsave()
                        .proactive
                        .finish(work, &result, clock());
                }
            }
            crate::sched::cond_resched();
        } else {
            // An early or check-to-sleep notification remains NOTIFIED in the
            // existing waiter protocol. Pending is always rechecked after wake.
            waiter
                .wait(false)
                .expect("background compaction wait failed");
        }
    }
}

// One actual buddy attempt; every successful probe has exclusive RAII ownership.
fn probe(work: Work) -> Result<Result<CompactedFrames, DmaAllocationFailure>, Stop> {
    Ok(LockedFrameAllocator::with_buddy(|buddy| {
        buddy.try_allocate_for_dma_at_level(work.count(), work.max, ReserveLevel::Kernel)
    })
    .ok_or(Stop::Busy)?
    .map(|(start, count)| CompactedFrames {
        start,
        pages: count.data(),
    }))
}

fn run(work: Work) -> Result<(), ServiceError> {
    let deadline = next_n_us_timer_jiffies(SLICE_US);
    let mut failure = match probe(work)? {
        Ok(run) => {
            drop(run);
            return Ok(());
        }
        Err(failure) => failure,
    };
    if matches!(
        failure.reason,
        AllocationFailure::Capacity
            | AllocationFailure::Reserve
            | AllocationFailure::ZoneLimit
            | AllocationFailure::AddressLimit
    ) {
        if clock() >= deadline {
            return Err(Stop::Timeout.into());
        }
        let zones = ZonePressureSnapshot {
            dma32_deficit: 1,
            normal_deficit: usize::from(work.max.data() > u32::MAX as usize),
        };
        let _ = PageReclaimer::try_shrink_global_below(work.count(), 4096, Some(zones), work.max);
        if clock() >= deadline {
            return Err(Stop::Timeout.into());
        }
        failure = match probe(work)? {
            Ok(run) => {
                drop(run);
                return Ok(());
            }
            Err(failure) => failure,
        };
    }
    let Some(zone) = failure
        .zone
        .filter(|_| failure.reason == AllocationFailure::Fragmentation)
    else {
        return Err(Stop::NoMemory.into());
    };
    if clock() >= deadline {
        return Err(Stop::Timeout.into());
    }
    #[cfg(feature = "mm_selftest")]
    selftest::COMPACTIONS.fetch_add(1, Ordering::Relaxed);
    let mut services = SERVICES.try_lock().map_err(|_| Stop::Busy)?;
    let outcome = services[work.slot].allocate(work.order(), work.max, zone, Some(deadline));
    #[cfg(feature = "mm_selftest")]
    selftest::MOVED.fetch_add(services[work.slot].last_moved as u64, Ordering::Relaxed);
    let run = outcome?;
    // Background work creates an ordinary free run, never a device-owned pool.
    drop(run);
    Ok(())
}

fn run_proactive(work: ProactiveWork) -> ProactiveResult {
    let result = |state, used_slice| ProactiveResult { state, used_slice };
    if clock() >= work.expires {
        return result(ProactiveState::Expired, false);
    }
    let normal = work.work.slot < MAX_COMPACT_ORDER;
    let sample = || -> Result<CompactionSnapshot, Stop> {
        LockedFrameAllocator::with_buddy(|buddy| {
            buddy.compaction_snapshot(work.work.order(), work.work.max, normal, HIGH_RUNS)
        })
        .ok_or(Stop::Busy)?
        .map_err(Stop::from)
    };
    let before = match sample() {
        Ok(value) => value,
        Err(stop) => return result(ProactiveState::Failed(stop), false),
    };
    let refilling = if work.refilling {
        before.runs < HIGH_RUNS
    } else {
        before.runs < LOW_RUNS
    };
    if !refilling {
        return result(ProactiveState::Healthy, false);
    }
    if clock() < work.slice_after {
        return result(ProactiveState::Cooling(work.slice_after), false);
    }
    let deadline = next_n_us_timer_jiffies(SLICE_US).min(work.expires);
    if clock() >= deadline {
        return result(ProactiveState::Expired, false);
    }
    let required = before
        .high
        .saturating_add(work.work.count().data())
        .saturating_add(8);
    if before.free_pages < before.high {
        crate::mm::page::reclaim_wait::request_pressure();
        return result(ProactiveState::Failed(Stop::NoMemory), false);
    }
    if !before.admitted || before.free_pages < required {
        let deficit = required.saturating_sub(before.free_pages).max(1);
        let zones = ZonePressureSnapshot {
            dma32_deficit: usize::from(!normal),
            normal_deficit: usize::from(normal),
        };
        let _ = PageReclaimer::try_shrink_global_below(
            PageFrameCount::new(deficit),
            256,
            Some(zones),
            work.work.max,
        );
        // Reclaim is one bounded slice; do not fold an additional migration
        // into it. A fresh later snapshot must re-establish headroom.
        return match sample() {
            Ok(after) if after.runs > before.runs => result(
                ProactiveState::Progress {
                    refilling: after.runs < HIGH_RUNS,
                },
                true,
            ),
            Ok(_) => result(ProactiveState::Failed(Stop::NoMemory), true),
            Err(stop) => result(ProactiveState::Failed(stop), true),
        };
    }
    let zone = if normal {
        DmaRecoveryZone::Normal
    } else {
        DmaRecoveryZone::Dma32
    };
    let mut services = match SERVICES.try_lock() {
        Ok(services) => services,
        Err(_) => return result(ProactiveState::Failed(Stop::Busy), false),
    };
    let outcome =
        services[work.work.slot].replenish(work.work.order(), work.work.max, zone, Some(deadline));
    #[cfg(feature = "mm_selftest")]
    selftest::PRO_MOVED.fetch_add(
        services[work.work.slot].last_moved as u64,
        Ordering::Relaxed,
    );
    drop(services);
    match outcome {
        Err(ServiceError::DiscoveryProgress) => result(ProactiveState::Discovery, false),
        Err(ServiceError::Stopped(stop)) => result(ProactiveState::Failed(stop), true),
        Ok(_) => match sample() {
            Ok(after) if after.runs > before.runs => result(
                ProactiveState::Progress {
                    refilling: after.runs < HIGH_RUNS,
                },
                true,
            ),
            Ok(_) => result(ProactiveState::Failed(Stop::Blocked), true),
            Err(stop) => result(ProactiveState::Failed(stop), true),
        },
    }
}

#[cfg(feature = "mm_selftest")]
pub(crate) mod selftest;
