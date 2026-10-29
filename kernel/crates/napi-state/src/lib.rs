#![no_std]

use core::sync::atomic::{AtomicU32, Ordering};

pub const SCHED: u32 = 1 << 0;
pub const MISSED: u32 = 1 << 1;
pub const DISABLE: u32 = 1 << 2;
pub const PAUSED: u32 = 1 << 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompleteState {
    Completed,
    Missed,
    Disabled,
    Paused,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleState {
    Acquired,
    Missed,
    Disabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeState {
    Acquired,
    Owned,
    Disabled,
}

/// Try to acquire the single NAPI poll owner.
///
/// A caller which acquires ownership must publish the NAPI instance exactly
/// once. A caller which observes an existing owner only records `MISSED`.
pub fn schedule_prep(state: &AtomicU32) -> ScheduleState {
    let mut current = state.load(Ordering::Acquire);
    loop {
        if current & (DISABLE | PAUSED) != 0 {
            return ScheduleState::Disabled;
        }

        let mut next = current | SCHED;
        if current & SCHED != 0 {
            next |= MISSED;
        }

        match state.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) if current & SCHED == 0 => return ScheduleState::Acquired,
            Ok(_) => return ScheduleState::Missed,
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
        if current & DISABLE != 0 {
            return CompleteState::Disabled;
        }
        debug_assert_ne!(current & SCHED, 0, "completing an unscheduled NAPI");

        if current & PAUSED != 0 {
            let next = current & !(SCHED | MISSED);
            match state.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return CompleteState::Paused,
                Err(observed) => {
                    current = observed;
                    continue;
                }
            }
        }

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

/// Reversibly stop new scheduling without stealing an existing poll owner.
pub fn pause(state: &AtomicU32) {
    state.fetch_or(PAUSED, Ordering::AcqRel);
}

/// Release a queued owner while administrative pause is still in effect.
/// `false` means resume won the race, so the caller must keep the owner.
pub fn complete_paused(state: &AtomicU32) -> bool {
    let mut current = state.load(Ordering::Acquire);
    loop {
        if current & PAUSED == 0 {
            return false;
        }
        debug_assert_ne!(current & SCHED, 0, "pausing an unscheduled NAPI owner");
        let next = current & !(SCHED | MISSED);
        match state.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

/// Resume a paused instance without creating a second queued/active owner.
pub fn resume(state: &AtomicU32) -> ResumeState {
    let mut current = state.load(Ordering::Acquire);
    loop {
        if current & DISABLE != 0 {
            return ResumeState::Disabled;
        }
        let (next, result) = if current & SCHED != 0 {
            ((current & !PAUSED) | MISSED, ResumeState::Owned)
        } else {
            ((current & !PAUSED) | SCHED, ResumeState::Acquired)
        };
        match state.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return result,
            Err(observed) => current = observed,
        }
    }
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
        assert_eq!(schedule_prep(&state), ScheduleState::Acquired);
        assert_eq!(state.load(Ordering::Relaxed), SCHED);
    }

    #[test]
    fn repeated_schedule_only_records_missed() {
        let state = AtomicU32::new(SCHED);
        assert_eq!(schedule_prep(&state), ScheduleState::Missed);
        assert_eq!(state.load(Ordering::Relaxed), SCHED | MISSED);
    }

    #[test]
    fn disable_rejects_schedule() {
        let state = AtomicU32::new(DISABLE);
        assert_eq!(schedule_prep(&state), ScheduleState::Disabled);
        assert_eq!(state.load(Ordering::Relaxed), DISABLE);
    }

    #[test]
    fn pause_rejects_schedule_until_resume_acquires_owner() {
        let state = AtomicU32::new(0);
        pause(&state);
        assert_eq!(schedule_prep(&state), ScheduleState::Disabled);
        assert_eq!(resume(&state), ResumeState::Acquired);
        assert_eq!(state.load(Ordering::Relaxed), SCHED);
    }

    #[test]
    fn paused_queued_owner_is_released_without_losing_pause() {
        let state = AtomicU32::new(SCHED | MISSED);
        pause(&state);
        assert!(complete_paused(&state));
        assert_eq!(state.load(Ordering::Relaxed), PAUSED);
    }

    #[test]
    fn resume_of_active_owner_records_missed() {
        let state = AtomicU32::new(SCHED | PAUSED);
        assert_eq!(resume(&state), ResumeState::Owned);
        assert_eq!(state.load(Ordering::Relaxed), SCHED | MISSED);
        assert_eq!(complete(&state), CompleteState::Missed);
    }

    #[test]
    fn completion_during_pause_releases_active_owner() {
        let state = AtomicU32::new(SCHED | PAUSED);
        assert_eq!(complete(&state), CompleteState::Paused);
        assert_eq!(state.load(Ordering::Relaxed), PAUSED);
    }

    #[test]
    fn resume_wins_queued_owner_race() {
        let state = AtomicU32::new(SCHED | PAUSED);
        assert_eq!(resume(&state), ResumeState::Owned);
        assert!(!complete_paused(&state));
        assert_eq!(state.load(Ordering::Relaxed), SCHED | MISSED);
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
            assert_eq!(schedule_prep(&worker_state), ScheduleState::Missed);
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
        assert_eq!(schedule_prep(&state), ScheduleState::Disabled);
        assert_eq!(complete(&state), CompleteState::Disabled);
        assert_eq!(state.load(Ordering::Acquire), DISABLE);
    }

    #[test]
    fn concurrent_disable_cannot_be_cleared_by_complete() {
        let state = Arc::new(AtomicU32::new(SCHED));
        let barrier = Arc::new(Barrier::new(2));
        let worker_state = state.clone();
        let worker_barrier = barrier.clone();

        let disabler = thread::spawn(move || {
            worker_barrier.wait();
            disable(&worker_state);
        });

        barrier.wait();
        disabler.join().unwrap();
        assert_eq!(complete(&state), CompleteState::Disabled);
        assert_eq!(state.load(Ordering::Acquire), DISABLE);
    }
}
