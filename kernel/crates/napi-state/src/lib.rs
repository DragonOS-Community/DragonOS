#![no_std]

use core::sync::atomic::{AtomicU32, Ordering};

pub const SCHED: u32 = 1 << 0;
pub const MISSED: u32 = 1 << 1;
pub const DISABLE: u32 = 1 << 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompleteState {
    Completed,
    Missed,
}

/// Try to acquire the single NAPI poll owner.
///
/// A caller which gets `true` owns the transition from idle to scheduled and
/// must publish the NAPI instance exactly once. A caller which observes an
/// existing owner only records `MISSED`.
pub fn schedule_prep(state: &AtomicU32) -> bool {
    let mut current = state.load(Ordering::Acquire);
    loop {
        if current & DISABLE != 0 {
            return false;
        }

        let mut next = current | SCHED;
        if current & SCHED != 0 {
            next |= MISSED;
        }

        match state.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return current & SCHED == 0,
            Err(observed) => current = observed,
        }
    }
}

/// Finish the current poll ownership transition.
///
/// If a scheduler recorded `MISSED`, ownership remains scheduled and the
/// caller must publish the instance for one more poll after completing any
/// device-specific callback handshake.
pub fn complete(state: &AtomicU32) -> CompleteState {
    let mut current = state.load(Ordering::Acquire);
    loop {
        debug_assert_ne!(current & SCHED, 0, "completing an unscheduled NAPI");

        let mut next = current & !(SCHED | MISSED);
        let result = if current & MISSED != 0 {
            next |= SCHED;
            CompleteState::Missed
        } else {
            CompleteState::Completed
        };

        match state.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return result,
            Err(observed) => current = observed,
        }
    }
}

/// Permanently disable a NAPI instance and release any outstanding ownership.
///
/// This is reserved for a backing interface which can no longer be polled.
/// Repeatedly calling `complete` is not equivalent: it creates an idle window
/// in which a concurrent scheduler can acquire an owner that nobody publishes.
pub fn disable(state: &AtomicU32) {
    state.store(DISABLE, Ordering::Release);
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn first_schedule_acquires_owner() {
        let state = AtomicU32::new(0);
        assert!(schedule_prep(&state));
        assert_eq!(state.load(Ordering::Relaxed), SCHED);
    }

    #[test]
    fn repeated_schedule_only_records_missed() {
        let state = AtomicU32::new(SCHED);
        assert!(!schedule_prep(&state));
        assert_eq!(state.load(Ordering::Relaxed), SCHED | MISSED);
    }

    #[test]
    fn disable_rejects_schedule() {
        let state = AtomicU32::new(DISABLE);
        assert!(!schedule_prep(&state));
        assert_eq!(state.load(Ordering::Relaxed), DISABLE);
    }

    #[test]
    fn complete_releases_idle_owner() {
        let state = AtomicU32::new(SCHED);
        assert_eq!(complete(&state), CompleteState::Completed);
        assert_eq!(state.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn complete_preserves_owner_after_missed() {
        let state = AtomicU32::new(SCHED | MISSED);
        assert_eq!(complete(&state), CompleteState::Missed);
        assert_eq!(state.load(Ordering::Relaxed), SCHED);
    }

    #[test]
    fn concurrent_schedule_is_not_lost_by_complete() {
        let state = Arc::new(AtomicU32::new(SCHED));
        let barrier = Arc::new(Barrier::new(2));
        let worker_state = state.clone();
        let worker_barrier = barrier.clone();

        let scheduler = thread::spawn(move || {
            worker_barrier.wait();
            assert!(!schedule_prep(&worker_state));
        });

        barrier.wait();
        scheduler.join().unwrap();
        assert_eq!(complete(&state), CompleteState::Missed);
        assert_eq!(state.load(Ordering::Relaxed), SCHED);
    }

    #[test]
    fn disable_releases_owner_and_rejects_future_schedule() {
        let state = AtomicU32::new(SCHED | MISSED);
        disable(&state);
        assert_eq!(state.load(Ordering::Acquire), DISABLE);
        assert!(!schedule_prep(&state));
    }
}
