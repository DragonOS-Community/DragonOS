#![no_std]

use core::sync::atomic::{AtomicU64, Ordering};

const DISARMED: u64 = 0;

/// Whether publishing a future protocol deadline requires the sleeping
/// scheduler to recompute its timeout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishResult {
    Unchanged,
    RearmRequired,
}

/// The result of atomically classifying a protocol deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DueResult {
    Disarmed,
    Future(u64),
    Claimed(u64),
}

/// The scheduler-owned future protocol deadline for one network interface.
///
/// Zero is reserved for the disarmed state. Immediate work is deliberately
/// not stored here: it remains owned by the caller currently polling the
/// protocol stack.
#[derive(Debug)]
pub struct PollDeadline(AtomicU64);

impl PollDeadline {
    pub const fn new() -> Self {
        Self(AtomicU64::new(DISARMED))
    }

    /// Publish a deadline which is strictly in the future.
    ///
    /// The returned decision is derived from the same RMW which publishes the
    /// value, so a concurrent claim cannot be hidden by a separate load/store
    /// sequence.
    pub fn publish_future(&self, now_us: u64, new_us: u64) -> PublishResult {
        debug_assert!(new_us > now_us);
        debug_assert_ne!(new_us, DISARMED);

        let old_us = self.0.swap(new_us, Ordering::AcqRel);
        if old_us == new_us {
            return PublishResult::Unchanged;
        }

        if old_us == DISARMED || old_us <= now_us || new_us < old_us {
            PublishResult::RearmRequired
        } else {
            PublishResult::Unchanged
        }
    }

    /// Remove the scheduler-owned deadline.
    ///
    /// A poller which already armed the old timeout may wake once early; no
    /// notification is needed because clearing cannot make work late.
    pub fn disarm(&self) {
        self.0.store(DISARMED, Ordering::Release);
    }

    /// Classify the current value and atomically claim it when due.
    ///
    /// CAS failure is handled internally so callers cannot accidentally sleep
    /// without reclassifying a concurrently replaced deadline.
    pub fn classify_and_claim(&self, now_us: u64) -> DueResult {
        let mut observed = self.0.load(Ordering::Acquire);
        loop {
            if observed == DISARMED {
                return DueResult::Disarmed;
            }
            if observed > now_us {
                return DueResult::Future(observed);
            }

            match self.0.compare_exchange_weak(
                observed,
                DISARMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return DueResult::Claimed(observed),
                Err(current) => observed = current,
            }
        }
    }

    /// Restore a claimed deadline only if no publisher has replaced it.
    pub fn restore_claimed_if_empty(&self, claimed_us: u64) -> bool {
        debug_assert_ne!(claimed_us, DISARMED);
        self.0
            .compare_exchange(DISARMED, claimed_us, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    #[inline]
    pub fn load(&self) -> Option<u64> {
        match self.0.load(Ordering::Acquire) {
            DISARMED => None,
            deadline => Some(deadline),
        }
    }
}

impl Default for PollDeadline {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn classifies_all_states_and_claims_due_once() {
        let deadline = PollDeadline::new();
        assert_eq!(deadline.classify_and_claim(100), DueResult::Disarmed);

        assert_eq!(
            deadline.publish_future(100, 200),
            PublishResult::RearmRequired
        );
        assert_eq!(deadline.classify_and_claim(199), DueResult::Future(200));
        assert_eq!(deadline.classify_and_claim(200), DueResult::Claimed(200));
        assert_eq!(deadline.classify_and_claim(200), DueResult::Disarmed);
    }

    #[test]
    fn only_new_or_earlier_deadlines_require_rearm() {
        let deadline = PollDeadline::new();
        assert_eq!(
            deadline.publish_future(10, 100),
            PublishResult::RearmRequired
        );
        assert_eq!(deadline.publish_future(10, 100), PublishResult::Unchanged);
        assert_eq!(deadline.publish_future(10, 120), PublishResult::Unchanged);
        assert_eq!(
            deadline.publish_future(10, 80),
            PublishResult::RearmRequired
        );
    }

    #[test]
    fn replacing_an_overdue_deadline_requires_reclassification() {
        let deadline = PollDeadline::new();
        assert_eq!(
            deadline.publish_future(10, 20),
            PublishResult::RearmRequired
        );
        assert_eq!(
            deadline.publish_future(30, 50),
            PublishResult::RearmRequired
        );
        assert_eq!(deadline.classify_and_claim(30), DueResult::Future(50));
    }

    #[test]
    fn disarm_clears_future_work() {
        let deadline = PollDeadline::new();
        deadline.publish_future(10, 20);
        deadline.disarm();
        assert_eq!(deadline.load(), None);
        assert_eq!(deadline.classify_and_claim(20), DueResult::Disarmed);
    }

    #[test]
    fn claim_before_publish_preserves_the_new_deadline() {
        let deadline = Arc::new(PollDeadline::new());
        deadline.publish_future(10, 20);

        let barrier = Arc::new(Barrier::new(2));
        let worker_deadline = deadline.clone();
        let worker_barrier = barrier.clone();
        let publisher = thread::spawn(move || {
            worker_barrier.wait();
            worker_deadline.publish_future(20, 40)
        });

        assert_eq!(deadline.classify_and_claim(20), DueResult::Claimed(20));
        barrier.wait();
        assert_eq!(publisher.join().unwrap(), PublishResult::RearmRequired);
        assert_eq!(deadline.classify_and_claim(20), DueResult::Future(40));
    }

    #[test]
    fn publish_before_claim_reclassifies_the_replacement() {
        let deadline = Arc::new(PollDeadline::new());
        deadline.publish_future(10, 20);

        let barrier = Arc::new(Barrier::new(2));
        let worker_deadline = deadline.clone();
        let worker_barrier = barrier.clone();
        let publisher = thread::spawn(move || {
            worker_deadline.publish_future(20, 40);
            worker_barrier.wait();
        });

        barrier.wait();
        assert_eq!(deadline.classify_and_claim(20), DueResult::Future(40));
        publisher.join().unwrap();
    }

    #[test]
    fn restore_never_overwrites_a_concurrent_publish() {
        let deadline = PollDeadline::new();
        deadline.publish_future(10, 20);
        assert_eq!(deadline.classify_and_claim(20), DueResult::Claimed(20));
        deadline.publish_future(20, 40);
        assert!(!deadline.restore_claimed_if_empty(20));
        assert_eq!(deadline.load(), Some(40));
    }

    #[test]
    fn one_of_concurrent_claimers_owns_the_due_value() {
        let deadline = Arc::new(PollDeadline::new());
        deadline.publish_future(10, 20);
        let barrier = Arc::new(Barrier::new(3));

        let spawn_claimer = |deadline: Arc<PollDeadline>, barrier: Arc<Barrier>| {
            thread::spawn(move || {
                barrier.wait();
                deadline.classify_and_claim(20)
            })
        };
        let first = spawn_claimer(deadline.clone(), barrier.clone());
        let second = spawn_claimer(deadline, barrier.clone());
        barrier.wait();

        let results = [first.join().unwrap(), second.join().unwrap()];
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == DueResult::Claimed(20))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == DueResult::Disarmed)
                .count(),
            1
        );
    }
}
