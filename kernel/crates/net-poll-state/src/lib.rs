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

/// The deadline sources atomically claimed for one interface poll.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeadlineClaims {
    protocol: Option<u64>,
    local_output: Option<u64>,
}

impl DeadlineClaims {
    fn is_empty(self) -> bool {
        self.protocol.is_none() && self.local_output.is_none()
    }
}

/// The result of atomically classifying an interface's deadlines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DueResult {
    Disarmed,
    Future(u64),
    Claimed {
        claims: DeadlineClaims,
        next: Option<u64>,
    },
}

/// One scheduler-owned future deadline source for a network interface.
///
/// Zero is reserved for the disarmed state. Immediate work is deliberately
/// not stored here: it remains owned by the caller currently polling the
/// protocol stack.
#[derive(Debug)]
struct AtomicDeadline(AtomicU64);

impl AtomicDeadline {
    const fn new() -> Self {
        Self(AtomicU64::new(DISARMED))
    }

    /// Publish a deadline which is strictly in the future.
    ///
    /// The returned decision is derived from the same RMW which publishes the
    /// value, so a concurrent claim cannot be hidden by a separate load/store
    /// sequence.
    fn publish_future(&self, now_us: u64, new_us: u64) -> PublishResult {
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

    /// Add a future deadline without postponing an earlier published one.
    ///
    /// This is used by independent work sources, such as device-backpressure
    /// retries, which share the protocol scheduler but do not own its current
    /// deadline. An overdue value is replaceable because the scheduler has not
    /// claimed it yet and must be notified to classify the new future value.
    fn publish_earlier_future(&self, now_us: u64, new_us: u64) -> PublishResult {
        debug_assert!(new_us > now_us);
        debug_assert_ne!(new_us, DISARMED);

        let mut observed = self.0.load(Ordering::Acquire);
        loop {
            if observed != DISARMED && observed > now_us && observed <= new_us {
                return PublishResult::Unchanged;
            }
            match self.0.compare_exchange_weak(
                observed,
                new_us,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return PublishResult::RearmRequired,
                Err(current) => observed = current,
            }
        }
    }

    /// Remove the scheduler-owned deadline.
    ///
    /// A poller which already armed the old timeout may wake once early; no
    /// notification is needed because clearing cannot make work late.
    fn disarm(&self) {
        self.0.store(DISARMED, Ordering::Release);
    }

    /// Classify the current value and atomically claim it when due.
    ///
    /// CAS failure is handled internally so callers cannot accidentally sleep
    /// without reclassifying a concurrently replaced deadline.
    fn classify_and_claim(&self, now_us: u64) -> AtomicDueResult {
        let mut observed = self.0.load(Ordering::Acquire);
        loop {
            if observed == DISARMED {
                return AtomicDueResult::Disarmed;
            }
            if observed > now_us {
                return AtomicDueResult::Future(observed);
            }

            match self.0.compare_exchange_weak(
                observed,
                DISARMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return AtomicDueResult::Claimed(observed),
                Err(current) => observed = current,
            }
        }
    }

    /// Restore a claimed deadline only if no publisher has replaced it.
    fn restore_claimed_if_empty(&self, claimed_us: u64) -> bool {
        debug_assert_ne!(claimed_us, DISARMED);
        self.0
            .compare_exchange(DISARMED, claimed_us, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    #[cfg(test)]
    #[inline]
    fn load(&self) -> Option<u64> {
        match self.0.load(Ordering::Acquire) {
            DISARMED => None,
            deadline => Some(deadline),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AtomicDueResult {
    Disarmed,
    Future(u64),
    Claimed(u64),
}

/// Scheduler deadlines for one network interface.
///
/// The protocol stack owns and may replace its deadline. Local-output retries
/// are an independent source and may only move their deadline earlier. Keeping
/// the sources separate prevents either owner from cancelling or postponing
/// the other's work while still exposing one earliest-deadline view to the
/// namespace scheduler.
#[derive(Debug)]
pub struct PollDeadlines {
    protocol: AtomicDeadline,
    local_output: AtomicDeadline,
}

impl PollDeadlines {
    pub const fn new() -> Self {
        Self {
            protocol: AtomicDeadline::new(),
            local_output: AtomicDeadline::new(),
        }
    }

    pub fn publish_protocol_future(&self, now_us: u64, new_us: u64) -> PublishResult {
        self.protocol.publish_future(now_us, new_us)
    }

    pub fn disarm_protocol(&self) {
        self.protocol.disarm();
    }

    pub fn publish_local_output_future(&self, now_us: u64, new_us: u64) -> PublishResult {
        self.local_output.publish_earlier_future(now_us, new_us)
    }

    pub fn classify_and_claim(&self, now_us: u64) -> DueResult {
        let mut claims = DeadlineClaims::default();
        let mut future = None;
        match self.protocol.classify_and_claim(now_us) {
            AtomicDueResult::Disarmed => {}
            AtomicDueResult::Future(deadline) => future = Some(deadline),
            AtomicDueResult::Claimed(deadline) => claims.protocol = Some(deadline),
        }
        match self.local_output.classify_and_claim(now_us) {
            AtomicDueResult::Disarmed => {}
            AtomicDueResult::Future(deadline) => {
                future = Some(future.map_or(deadline, |current| current.min(deadline)));
            }
            AtomicDueResult::Claimed(deadline) => claims.local_output = Some(deadline),
        }

        if !claims.is_empty() {
            DueResult::Claimed {
                claims,
                next: future,
            }
        } else if let Some(deadline) = future {
            DueResult::Future(deadline)
        } else {
            DueResult::Disarmed
        }
    }

    pub fn restore_claimed_if_empty(&self, claims: DeadlineClaims) -> bool {
        let protocol_restored = claims
            .protocol
            .is_some_and(|deadline| self.protocol.restore_claimed_if_empty(deadline));
        let local_output_restored = claims
            .local_output
            .is_some_and(|deadline| self.local_output.restore_claimed_if_empty(deadline));
        protocol_restored || local_output_restored
    }

    #[cfg(test)]
    fn load(&self) -> Option<u64> {
        match (self.protocol.load(), self.local_output.load()) {
            (Some(protocol), Some(local_output)) => Some(protocol.min(local_output)),
            (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
            (None, None) => None,
        }
    }
}

impl Default for PollDeadlines {
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
        let deadline = AtomicDeadline::new();
        assert_eq!(deadline.classify_and_claim(100), AtomicDueResult::Disarmed);

        assert_eq!(
            deadline.publish_future(100, 200),
            PublishResult::RearmRequired
        );
        assert_eq!(
            deadline.classify_and_claim(199),
            AtomicDueResult::Future(200)
        );
        assert_eq!(
            deadline.classify_and_claim(200),
            AtomicDueResult::Claimed(200)
        );
        assert_eq!(deadline.classify_and_claim(200), AtomicDueResult::Disarmed);
    }

    #[test]
    fn only_new_or_earlier_deadlines_require_rearm() {
        let deadline = AtomicDeadline::new();
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
        let deadline = AtomicDeadline::new();
        assert_eq!(
            deadline.publish_future(10, 20),
            PublishResult::RearmRequired
        );
        assert_eq!(
            deadline.publish_future(30, 50),
            PublishResult::RearmRequired
        );
        assert_eq!(deadline.classify_and_claim(30), AtomicDueResult::Future(50));
    }

    #[test]
    fn additive_publish_never_postpones_an_earlier_deadline() {
        let deadline = AtomicDeadline::new();
        assert_eq!(
            deadline.publish_earlier_future(10, 100),
            PublishResult::RearmRequired
        );
        assert_eq!(
            deadline.publish_earlier_future(10, 120),
            PublishResult::Unchanged
        );
        assert_eq!(deadline.load(), Some(100));
        assert_eq!(
            deadline.publish_earlier_future(10, 80),
            PublishResult::RearmRequired
        );
        assert_eq!(deadline.load(), Some(80));
    }

    #[test]
    fn additive_publish_replaces_an_unclaimed_overdue_deadline() {
        let deadline = AtomicDeadline::new();
        deadline.publish_future(10, 20);
        assert_eq!(
            deadline.publish_earlier_future(30, 50),
            PublishResult::RearmRequired
        );
        assert_eq!(deadline.classify_and_claim(30), AtomicDueResult::Future(50));
    }

    #[test]
    fn disarm_clears_future_work() {
        let deadline = AtomicDeadline::new();
        deadline.publish_future(10, 20);
        deadline.disarm();
        assert_eq!(deadline.load(), None);
        assert_eq!(deadline.classify_and_claim(20), AtomicDueResult::Disarmed);
    }

    #[test]
    fn claim_before_publish_preserves_the_new_deadline() {
        let deadline = Arc::new(AtomicDeadline::new());
        deadline.publish_future(10, 20);

        let barrier = Arc::new(Barrier::new(2));
        let worker_deadline = deadline.clone();
        let worker_barrier = barrier.clone();
        let publisher = thread::spawn(move || {
            worker_barrier.wait();
            worker_deadline.publish_future(20, 40)
        });

        assert_eq!(
            deadline.classify_and_claim(20),
            AtomicDueResult::Claimed(20)
        );
        barrier.wait();
        assert_eq!(publisher.join().unwrap(), PublishResult::RearmRequired);
        assert_eq!(deadline.classify_and_claim(20), AtomicDueResult::Future(40));
    }

    #[test]
    fn publish_before_claim_reclassifies_the_replacement() {
        let deadline = Arc::new(AtomicDeadline::new());
        deadline.publish_future(10, 20);

        let barrier = Arc::new(Barrier::new(2));
        let worker_deadline = deadline.clone();
        let worker_barrier = barrier.clone();
        let publisher = thread::spawn(move || {
            worker_deadline.publish_future(20, 40);
            worker_barrier.wait();
        });

        barrier.wait();
        assert_eq!(deadline.classify_and_claim(20), AtomicDueResult::Future(40));
        publisher.join().unwrap();
    }

    #[test]
    fn restore_never_overwrites_a_concurrent_publish() {
        let deadline = AtomicDeadline::new();
        deadline.publish_future(10, 20);
        assert_eq!(
            deadline.classify_and_claim(20),
            AtomicDueResult::Claimed(20)
        );
        deadline.publish_future(20, 40);
        assert!(!deadline.restore_claimed_if_empty(20));
        assert_eq!(deadline.load(), Some(40));
    }

    #[test]
    fn one_of_concurrent_claimers_owns_the_due_value() {
        let deadline = Arc::new(AtomicDeadline::new());
        deadline.publish_future(10, 20);
        let barrier = Arc::new(Barrier::new(3));

        let spawn_claimer = |deadline: Arc<AtomicDeadline>, barrier: Arc<Barrier>| {
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
                .filter(|result| **result == AtomicDueResult::Claimed(20))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == AtomicDueResult::Disarmed)
                .count(),
            1
        );
    }

    #[test]
    fn protocol_updates_cannot_postpone_or_cancel_local_output() {
        let deadlines = PollDeadlines::new();
        deadlines.publish_local_output_future(10, 20);
        deadlines.publish_protocol_future(10, 40);
        assert_eq!(deadlines.classify_and_claim(10), DueResult::Future(20));

        deadlines.disarm_protocol();
        assert_eq!(deadlines.classify_and_claim(10), DueResult::Future(20));
    }

    #[test]
    fn claims_and_restores_each_due_source_together() {
        let deadlines = PollDeadlines::new();
        deadlines.publish_protocol_future(10, 20);
        deadlines.publish_local_output_future(10, 20);

        let DueResult::Claimed { claims, next } = deadlines.classify_and_claim(20) else {
            panic!("both deadline sources must be claimed");
        };
        assert_eq!(next, None);
        assert_eq!(deadlines.load(), None);
        assert!(deadlines.restore_claimed_if_empty(claims));
        assert_eq!(deadlines.classify_and_claim(19), DueResult::Future(20));
    }

    #[test]
    fn protocol_claim_preserves_local_output_future() {
        let deadlines = PollDeadlines::new();
        deadlines.publish_protocol_future(10, 20);
        deadlines.publish_local_output_future(10, 40);

        let DueResult::Claimed { claims, next } = deadlines.classify_and_claim(20) else {
            panic!("protocol deadline must be claimed");
        };
        assert_eq!(next, Some(40));
        assert_eq!(claims.protocol, Some(20));
        assert_eq!(claims.local_output, None);
    }

    #[test]
    fn local_output_claim_preserves_protocol_future() {
        let deadlines = PollDeadlines::new();
        deadlines.publish_local_output_future(10, 20);
        deadlines.publish_protocol_future(10, 40);

        let DueResult::Claimed { claims, next } = deadlines.classify_and_claim(20) else {
            panic!("local-output deadline must be claimed");
        };
        assert_eq!(next, Some(40));
        assert_eq!(claims.protocol, None);
        assert_eq!(claims.local_output, Some(20));
    }
}
