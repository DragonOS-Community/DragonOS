//! One-shot background work authorized by real failed high-order DMA requests.
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
            buddy::{AllocationFailure, DmaAllocationFailure, DmaRecoveryZone},
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
}
impl Pending {
    const fn new() -> Self {
        Self {
            slots: [Slot::new(); SLOTS],
            next: 0,
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

static PENDING: SpinLock<Pending> = SpinLock::new(Pending::new());
// Permanent storage, not a large service array on the worker's kernel stack.
static SERVICES: Mutex<[Service; SLOTS]> = Mutex::new([const { Service::new() }; SLOTS]);
static NEXT_DEADLINE: AtomicU64 = AtomicU64::new(0);
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

/// Timer IRQ only queues a deferred notification; it never consumes the slot.
pub(crate) fn tick(now: u64) {
    let deadline = NEXT_DEADLINE.load(Ordering::Acquire);
    if deadline != 0 && now >= deadline {
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
    loop {
        let work = {
            let mut pending = PENDING.lock_irqsave();
            let work = pending.take_ready(clock());
            // Old due deadlines must not keep firing during a recovery slice.
            NEXT_DEADLINE.store(
                if work.is_some() {
                    0
                } else {
                    pending.deadline()
                },
                Ordering::Release,
            );
            work
        };
        if let Some(work) = work {
            #[cfg(feature = "mm_selftest")]
            selftest::ROUNDS.fetch_add(1, Ordering::Relaxed);
            let outcome = run(work);
            #[cfg(feature = "mm_selftest")]
            selftest::record_stop(&outcome);
            #[cfg(feature = "mm_selftest")]
            if outcome.is_ok() {
                selftest::SUCCESSES.fetch_add(1, Ordering::Relaxed);
            }
            PENDING.lock_irqsave().finish(work, &outcome);
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

#[cfg(feature = "mm_selftest")]
pub(crate) mod selftest;
