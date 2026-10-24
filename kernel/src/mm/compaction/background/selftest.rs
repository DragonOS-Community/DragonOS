//! Local protocol cases and passive observations of the real worker.
//! No switch pauses the production worker or fabricates a DMA failure.
use super::*;
use core::sync::atomic::AtomicU64;

pub(super) static REQUESTS: AtomicU64 = AtomicU64::new(0);
pub(super) static ROUNDS: AtomicU64 = AtomicU64::new(0);
pub(super) static SUCCESSES: AtomicU64 = AtomicU64::new(0);
pub(super) static COMPACTIONS: AtomicU64 = AtomicU64::new(0);
pub(super) static DISCOVERY_PROGRESS: AtomicU64 = AtomicU64::new(0);
pub(super) static MOVED: AtomicU64 = AtomicU64::new(0);
static STOPS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

pub(super) fn record_stop(outcome: &Result<(), ServiceError>) {
    let index = match outcome {
        Ok(()) => return,
        Err(ServiceError::DiscoveryProgress) => {
            DISCOVERY_PROGRESS.fetch_add(1, Ordering::Relaxed);
            return;
        }
        Err(ServiceError::Stopped(Stop::Invalid)) => 0,
        Err(ServiceError::Stopped(Stop::Busy)) => 1,
        Err(ServiceError::Stopped(Stop::Budget)) => 2,
        Err(ServiceError::Stopped(Stop::Blocked)) => 3,
        Err(ServiceError::Stopped(Stop::NoMemory)) => 4,
        Err(ServiceError::Stopped(Stop::Deferred)) => 5,
        Err(ServiceError::Stopped(Stop::Timeout)) => 6,
    };
    STOPS[index].fetch_add(1, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Observation {
    pub requests: u64,
    pub rounds: u64,
    pub successes: u64,
    pub compactions: u64,
    pub moved: u64,
    pub discovery_progress: u64,
    /// Invalid, Busy, Budget, Blocked, NoMemory, Deferred, Timeout.
    pub stops: [u64; 7],
    pub pending: usize,
    pub active: usize,
    pub deadline: u64,
}

/// Use around a real DMA fixture to await actual consume/idle, not to authorize
/// background work. Original requests must still pass production eligibility.
pub(crate) fn observe() -> Observation {
    let state = PENDING.lock_irqsave();
    Observation {
        requests: REQUESTS.load(Ordering::Relaxed),
        rounds: ROUNDS.load(Ordering::Relaxed),
        successes: SUCCESSES.load(Ordering::Relaxed),
        compactions: COMPACTIONS.load(Ordering::Relaxed),
        moved: MOVED.load(Ordering::Relaxed),
        discovery_progress: DISCOVERY_PROGRESS.load(Ordering::Relaxed),
        stops: core::array::from_fn(|index| STOPS[index].load(Ordering::Relaxed)),
        pending: state
            .slots
            .iter()
            .map(|slot| match slot.pending {
                Some((low, high)) if low != high => 2,
                Some(_) => 1,
                None => 0,
            })
            .sum(),
        active: state.slots.iter().filter(|slot| slot.active).count(),
        deadline: NEXT_DEADLINE.load(Ordering::Acquire),
    }
}

fn endpoint_fairness() -> bool {
    let mut state = Pending::new();
    if !state.post(0, 100) || state.post(0, 1000) {
        return false;
    }
    let Some(narrow) = state.take_ready(0) else {
        return false;
    };
    if narrow.max.data() != 100 {
        return false;
    }
    // Repeated narrow arrivals while that demand is active cannot hide the
    // original wide endpoint or create extra queue entries.
    for _ in 0..64 {
        state.post(0, 10);
    }
    state.finish(narrow, &Err(ServiceError::Stopped(Stop::Blocked)));
    let Some(wide) = state.take_ready(u64::MAX) else {
        return false;
    };
    let good = wide.max.data() == 1000 && state.slots[0].pending == Some((10, 10));
    state.finish(wide, &Ok(()));
    let Some(last) = state.take_ready(u64::MAX) else {
        return false;
    };
    good && last.max.data() == 10 && state.slots[0].pending.is_none()
}

fn active_arrival_and_cooldown() -> bool {
    let mut state = Pending::new();
    state.post(0, 100);
    let Some(work) = state.take_ready(0) else {
        return false;
    };
    // No wake is required while active: completion must recheck new pending.
    if state.post(0, 500) {
        return false;
    }
    let before = clock();
    state.finish(work, &Ok(()));
    let earliest = state.slots[0].not_before;
    if earliest <= before || state.take_ready(earliest - 1).is_some() {
        return false;
    }
    if state.deadline() != earliest {
        return false;
    }
    let Some(next) = state.take_ready(earliest) else {
        return false;
    };
    if next.max.data() != 500 {
        return false;
    }
    state.finish(next, &Err(ServiceError::Stopped(Stop::Blocked)));
    let cold = state.slots[0].not_before;
    state.post(0, 1);
    state.slots[0].not_before == cold && state.deadline() == cold
}

fn round_robin_and_idle() -> bool {
    let mut state = Pending::new();
    if state.deadline() != 0 || state.take_ready(0).is_some() {
        return false;
    }
    state.post(0, 200);
    state.post(1, 300);
    let Some(first) = state.take_ready(0) else {
        return false;
    };
    state.post(0, 100);
    let Some(second) = state.take_ready(0) else {
        return false;
    };
    if first.slot != 0 || second.slot != 1 {
        return false;
    }
    state.finish(first, &Err(ServiceError::Stopped(Stop::Blocked)));
    state.finish(second, &Ok(()));
    let Some(last) = state.take_ready(u64::MAX) else {
        return false;
    };
    state.finish(last, &Err(ServiceError::Stopped(Stop::Blocked)));
    state.deadline() == 0 && state.take_ready(u64::MAX).is_none()
}

fn discovery_cooldown() -> bool {
    let mut state = Pending::new();
    for _ in 0..3 {
        state.post(0, 100);
        let Some(work) = state.take_ready(u64::MAX) else {
            return false;
        };
        state.finish(work, &Err(ServiceError::Stopped(Stop::Blocked)));
    }
    if state.slots[0].failures != 3 {
        return false;
    }
    state.post(0, 100);
    let Some(work) = state.take_ready(u64::MAX) else {
        return false;
    };
    // A fresh arrival while active must survive finish, including progress.
    state.post(0, 200);
    let earliest = next_n_us_timer_jiffies(MIN_INTERVAL_US);
    state.finish(work, &Err(ServiceError::DiscoveryProgress));
    let latest = next_n_us_timer_jiffies(MIN_INTERVAL_US);
    let ready_at = state.slots[0].not_before;
    if state.slots[0].failures != 3
        || ready_at < earliest
        || ready_at > latest
        || state.take_ready(ready_at - 1).is_some()
        || state.deadline() != ready_at
    {
        return false;
    }
    let Some(next) = state.take_ready(ready_at) else {
        return false;
    };
    if next.max.data() != 200 {
        return false;
    }
    // Progress leaves actual failure history intact; another real failure
    // advances that history rather than restarting its exponential backoff.
    state.finish(next, &Err(ServiceError::Stopped(Stop::Blocked)));
    if state.slots[0].failures != 4 || state.deadline() != 0 || state.take_ready(u64::MAX).is_some()
    {
        return false;
    }
    state.post(0, 300);
    let Some(work) = state.take_ready(u64::MAX) else {
        return false;
    };
    state.finish(work, &Err(ServiceError::DiscoveryProgress));
    if state.slots[0].failures != 4 || state.deadline() != 0 || state.take_ready(u64::MAX).is_some()
    {
        return false;
    }
    state.post(0, 400);
    let Some(work) = state.take_ready(u64::MAX) else {
        return false;
    };
    state.finish(work, &Ok(()));
    state.slots[0].failures == 0 && state.deadline() == 0 && state.take_ready(u64::MAX).is_none()
}

pub(crate) fn checks(buckets: usize) -> [(&'static str, bool); 5] {
    [
        ("background_endpoint_fairness", endpoint_fairness()),
        ("background_arrival_cooldown", active_arrival_and_cooldown()),
        ("background_round_robin_idle", round_robin_and_idle()),
        (
            "background_discovery_boundaries",
            super::super::recovery::selftest::discovery_boundaries(buckets),
        ),
        ("background_discovery_cooldown", discovery_cooldown()),
    ]
}

pub(super) static PRO_DEMANDS: AtomicU64 = AtomicU64::new(0);
static PRO_ROUNDS: AtomicU64 = AtomicU64::new(0);
static PRO_SLOT_ROUNDS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

pub(super) fn record_proactive_start(slot: usize) {
    PRO_ROUNDS.fetch_add(1, Ordering::Relaxed);
    PRO_SLOT_ROUNDS[slot].fetch_add(1, Ordering::Relaxed);
}
pub(super) static PRO_MOVED: AtomicU64 = AtomicU64::new(0);
static PRO_SLICES: AtomicU64 = AtomicU64::new(0);
static PRO_DISCOVERIES: AtomicU64 = AtomicU64::new(0);
static PRO_INCREASED: AtomicU64 = AtomicU64::new(0);
static PRO_FAILURES: AtomicU64 = AtomicU64::new(0);
static PRO_STOPS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

pub(super) fn record_proactive(result: &ProactiveResult) {
    if result.used_slice {
        PRO_SLICES.fetch_add(1, Ordering::Relaxed);
    }
    match result.state {
        ProactiveState::Discovery => {
            PRO_DISCOVERIES.fetch_add(1, Ordering::Relaxed);
        }
        ProactiveState::Progress { .. } => {
            PRO_INCREASED.fetch_add(1, Ordering::Relaxed);
        }
        ProactiveState::Failed(stop) => {
            PRO_FAILURES.fetch_add(1, Ordering::Relaxed);
            let index = match stop {
                Stop::Invalid => 0,
                Stop::Busy => 1,
                Stop::Budget => 2,
                Stop::Blocked => 3,
                Stop::NoMemory => 4,
                Stop::Deferred => 5,
                Stop::Timeout => 6,
            };
            PRO_STOPS[index].fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProactiveObservation {
    pub demands: u64,
    pub rounds: u64,
    pub slices: u64,
    pub discoveries: u64,
    pub moved: u64,
    pub increased: u64,
    pub failures: u64,
    pub expired: u64,
    pub stops: [u64; 7],
    pub active: usize,
    pub leases: usize,
    pub deadline: u64,
}

pub(crate) fn observe_proactive() -> ProactiveObservation {
    let state = PENDING.lock_irqsave();
    let now = clock();
    ProactiveObservation {
        demands: PRO_DEMANDS.load(Ordering::Relaxed),
        rounds: PRO_ROUNDS.load(Ordering::Relaxed),
        slices: PRO_SLICES.load(Ordering::Relaxed),
        discoveries: PRO_DISCOVERIES.load(Ordering::Relaxed),
        moved: PRO_MOVED.load(Ordering::Relaxed),
        increased: PRO_INCREASED.load(Ordering::Relaxed),
        failures: PRO_FAILURES.load(Ordering::Relaxed),
        expired: state.proactive.expired,
        stops: core::array::from_fn(|index| PRO_STOPS[index].load(Ordering::Relaxed)),
        active: state
            .proactive
            .slots
            .iter()
            .filter(|slot| slot.active)
            .count(),
        leases: state
            .proactive
            .slots
            .iter()
            .flat_map(|slot| slot.endpoints.iter().flatten())
            .filter(|demand| demand.expires > now)
            .count(),
        deadline: PROACTIVE_DEADLINE.load(Ordering::Acquire),
    }
}

/// Observe one slot without treating other slots' legitimate demand as work
/// authorized by this slot. The worker increments rounds only after taking it.
pub(crate) fn observe_slot(order: usize, normal: bool) -> Option<(u64, usize)> {
    if order == 0 || order > MAX_COMPACT_ORDER {
        return None;
    }
    let index = usize::from(!normal) * MAX_COMPACT_ORDER + order - 1;
    let state = PENDING.lock_irqsave();
    let now = clock();
    let leases = state.proactive.slots[index]
        .endpoints
        .iter()
        .flatten()
        .filter(|demand| demand.expires > now)
        .count();
    Some((PRO_SLOT_ROUNDS[index].load(Ordering::Relaxed), leases))
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct MaskObservation {
    pub expires: u64,
    pub refilling: bool,
    pub not_before: u64,
}

pub(crate) fn observe_mask(order: usize, max: PhysAddr, normal: bool) -> Option<MaskObservation> {
    if order == 0 || order > MAX_COMPACT_ORDER {
        return None;
    }
    let max = if normal {
        max.data()
    } else {
        max.data().min(u32::MAX as usize)
    };
    let state = PENDING.lock_irqsave();
    let slot = &state.proactive.slots[usize::from(!normal) * MAX_COMPACT_ORDER + order - 1];
    let demand = slot
        .endpoints
        .iter()
        .flatten()
        .find(|demand| demand.max == max && demand.expires > clock())?;
    Some(MaskObservation {
        expires: demand.expires,
        refilling: demand.refilling,
        not_before: demand
            .check_after
            .max(slot.not_before)
            .max(state.proactive.discovery_after),
    })
}

fn recent_masks() -> bool {
    let mut state = Proactive::new();
    state.post(0, 100, 0);
    state.post(0, 200, 1);
    let old_wide = state.slots[0].endpoints[1].unwrap().expires;
    state.post(0, 100, 2);
    if state.slots[0].endpoints[1].unwrap().expires != old_wide {
        return false;
    }
    state.post(0, 300, 3);
    if !state.slots[0]
        .endpoints
        .iter()
        .flatten()
        .any(|entry| entry.max == 100)
        || state.slots[0]
            .endpoints
            .iter()
            .flatten()
            .any(|entry| entry.max == 200)
    {
        return false;
    }
    state.expire(after(3, LEASE_US));
    state.deadline() == 0 && state.expired == 2
}

fn proactive_gates() -> bool {
    let mut state = Proactive::new();
    state.post(0, 100, 0);
    state.post(1, 200, 0);
    let now = after(0, CHECK_US);
    let Some(first) = state.take_ready(now) else {
        return false;
    };
    state.finish(
        first,
        &ProactiveResult {
            state: ProactiveState::Healthy,
            used_slice: false,
        },
        now,
    );
    if state.slice_after != 0 || state.take_ready(now).is_some() {
        return false;
    }
    let next = after(now, DISCOVERY_US);
    let Some(second) = state.take_ready(next) else {
        return false;
    };
    if second.work.slot != 1 {
        return false;
    }
    state.finish(
        second,
        &ProactiveResult {
            state: ProactiveState::Failed(Stop::Blocked),
            used_slice: true,
        },
        next,
    );
    let gate = state.slice_after;
    let cooldown = state.slots[1].not_before;
    state.post(1, 300, next + 1);
    state.slice_after == gate
        && state.slots[1].not_before == cooldown
        && state.slots[1].failures == 1
}

fn proactive_backoff_expiry() -> bool {
    let mut state = Proactive::new();
    state.post(0, 100, 0);
    let mut now = after(0, CHECK_US);
    // Repeated real successes keep the lease fresh without clearing failures.
    for expected in 1..=7 {
        while state.slots[0].endpoints[0].unwrap().expires <= now {
            let refresh = state.slots[0].endpoints[0].unwrap().expires - 1;
            state.post(0, 100, refresh);
        }
        state.post(0, 100, now);
        let Some(work) = state.take_ready(now) else {
            return false;
        };
        state.finish(
            work,
            &ProactiveResult {
                state: ProactiveState::Failed(Stop::Blocked),
                used_slice: true,
            },
            now,
        );
        if state.slots[0].failures != expected {
            return false;
        }
        now = state.slots[0].not_before;
    }
    let expires = state.slots[0].endpoints[0].unwrap().expires;
    if state.deadline() != expires || expires >= now {
        return false;
    }
    state.expire(expires);
    state.deadline() == 0 && state.take_ready(now).is_none()
}

fn proactive_replacement() -> bool {
    let mut state = Proactive::new();
    state.post(0, 100, 0);
    state.post(0, 200, 1);
    let now = after(0, CHECK_US);
    let Some(work) = state.take_ready(now) else {
        return false;
    };
    state.post(0, 300, now);
    state.finish(
        work,
        &ProactiveResult {
            state: ProactiveState::Progress { refilling: true },
            used_slice: true,
        },
        now,
    );
    state.slots[0].endpoints[work.endpoint]
        .is_some_and(|entry| entry.max == 300 && !entry.refilling)
}

fn proactive_renewal() -> bool {
    let mut state = Proactive::new();
    state.post(0, 100, 0);
    let Some(work) = state.take_ready(after(0, CHECK_US)) else {
        return false;
    };
    // The worker is delayed after taking the old lease; a real success renews
    // the same mask before its original expiry, then the old round expires.
    state.post(0, 100, work.expires - 1);
    let renewed = state.slots[0].endpoints[work.endpoint].unwrap().expires;
    state.finish(
        work,
        &ProactiveResult {
            state: ProactiveState::Expired,
            used_slice: false,
        },
        work.expires,
    );
    let Some(endpoint) = state.slots[0].endpoints[work.endpoint] else {
        return false;
    };
    let ready = after(work.expires, DISCOVERY_US);
    endpoint.expires == renewed
        && renewed > work.expires
        && !endpoint.refilling
        && endpoint.check_after == ready
        && state.deadline() == ready
        && state.expired == 0
        && state
            .take_ready(ready)
            .is_some_and(|next| next.expires == renewed)
}

fn work_class_fairness() -> bool {
    let mut state = Pending::new();
    state.post(0, 100);
    state.proactive.post(1, 200, 0);
    let now = after(0, CHECK_US);
    let Some(SelectedWork::Urgent(_)) = state.take_next(now) else {
        return false;
    };
    state.post(2, 300);
    matches!(state.take_next(now), Some(SelectedWork::Proactive(_)))
}

pub(crate) fn proactive_checks() -> [(&'static str, bool); 5] {
    [
        ("proactive_recent_masks", recent_masks()),
        ("proactive_global_gates", proactive_gates()),
        ("proactive_backoff_expiry", proactive_backoff_expiry()),
        (
            "proactive_active_replacement",
            proactive_replacement() && proactive_renewal(),
        ),
        ("proactive_class_fairness", work_class_fairness()),
    ]
}

pub(super) static INTERNAL_SUPPRESSED: AtomicU64 = AtomicU64::new(0);
static WORKER_ALLOCATION_CHECK: AtomicU64 = AtomicU64::new(0);

/// A single real order-one allocation by the real worker proves that its
/// higher-order metadata hook reaches the owner filter. Other tasks may post
/// legitimate demand concurrently, so a global-demand delta is not an oracle.
pub(super) fn check_worker_allocation() {
    use crate::mm::allocator::kernel_allocator::{KernelAllocator, LocalAlloc};
    let layout = core::alloc::Layout::from_size_align(4097, 4096).unwrap();
    let before = INTERNAL_SUPPRESSED.load(Ordering::Relaxed);
    let pointer = unsafe { KernelAllocator.alloc_physical(layout, false, true) };
    let passed = !pointer.is_null() && INTERNAL_SUPPRESSED.load(Ordering::Relaxed) > before;
    if !pointer.is_null() {
        unsafe {
            KernelAllocator.local_dealloc(pointer, layout);
        }
    }
    WORKER_ALLOCATION_CHECK.store(if passed { 1 } else { 2 }, Ordering::Release);
}

/// None until the real worker has completed its one-time allocation check.
pub(crate) fn worker_allocation_check() -> Option<bool> {
    match WORKER_ALLOCATION_CHECK.load(Ordering::Acquire) {
        0 => None,
        1 => Some(true),
        _ => Some(false),
    }
}
