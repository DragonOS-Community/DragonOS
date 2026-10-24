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
