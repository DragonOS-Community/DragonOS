//! Prepared OOM reaper storage and single-step execution.
//!
//! Only a committed victim may claim its MM's slot. Claiming and
//! queue operations never allocate; execution requires the separate MM-bound
//! authorization consumed by the real nonblocking MM reaping implementation.

use alloc::sync::Arc;
use core::{alloc::AllocError, fmt, sync::atomic::Ordering};

use crate::{
    cgroup::memory::{
        accounting::{ChargeFailure, KernelByteKind},
        object::ObjectAccount,
        owned::OwnedBox,
    },
    libs::spinlock::SpinLock,
};

use super::{
    ucontext::{
        oom_reap::{OomReapAuthorization, OomReapBatchResult},
        AddressSpace,
    },
    VirtAddr,
};

const MAX_COLLISIONS: u8 = 10;
const MAX_FOREIGN_RESTARTS: u8 = 10;
const COLLISION_DELAY_NS: u64 = 100_000_000;
const EXIT_GRACE_NS: u64 = 2_000_000_000;

#[cfg(not(test))]
#[path = "oom_reaper_worker.rs"]
mod worker;
#[cfg(not(test))]
pub(super) use worker::{queue_committed_mm, wake_worker};

#[derive(Debug)]
pub(crate) enum JobBirthError {
    Charge(ChargeFailure),
    Physical(AllocError),
}

/// Completion of the scan is not completion of mmput or physical release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReapFinish {
    Scanned,
    Protected,
    UnsupportedMapping,
    CollisionLimit,
    ForeignMutationLimit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReapStatus {
    Idle,
    Queued,
    Reaping,
    Finished(ReapFinish),
}

struct SlotState {
    status: ReapStatus,
    spare: Option<OwnedBox<OomReapJob>>,
}

/// Intended to be embedded in the matching MM after typed birth is connected.
pub(crate) struct OomReapSlot {
    mm_id: u64,
    state: SpinLock<SlotState>,
}

impl fmt::Debug for OomReapSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OomReapSlot")
            .field("mm_id", &self.mm_id)
            .field("status", &self.status())
            .finish()
    }
}

/// Preparation before a constructor enters parent-MM/exec locks.
/// No MM identity is allocated or guessed until the consuming bind step.
pub(crate) struct PreparedOomReapSlot {
    spare: OwnedBox<OomReapJob>,
}

impl PreparedOomReapSlot {
    pub(crate) fn try_new(creator: &Arc<ObjectAccount>) -> Result<Self, JobBirthError> {
        let spare = OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)
            .map_err(JobBirthError::Charge)?
            .try_init(OomReapJob {
                mm: None,
                authorization: None,
                next: None,
                cursor: VirtAddr::new(0),
                expected_gen: 0,
                collisions: 0,
                foreign_restarts: 0,
                not_before_ns: 0,
            })
            .map_err(JobBirthError::Physical)?;
        Ok(Self { spare })
    }

    /// Only attach the already prepared storage to the actual new MM id.
    /// This cannot allocate, charge, register work or hold a self-MM Arc.
    pub(crate) fn bind(self, mm_id: u64) -> OomReapSlot {
        OomReapSlot {
            mm_id,
            state: SpinLock::new(SlotState {
                status: ReapStatus::Idle,
                spare: Some(self.spare),
            }),
        }
    }
}

impl OomReapSlot {
    pub(crate) fn status(&self) -> ReapStatus {
        self.state.lock_irqsave().status
    }

    /// Called only for an actual committed kill of this exact MM. This does
    /// consumes the publisher's permission but never creates one or publishes work.
    pub(crate) fn claim_after_commit(
        &self,
        mm: &Arc<AddressSpace>,
        authorization: OomReapAuthorization,
        committed_at_ns: u64,
    ) -> Option<OwnedBox<OomReapJob>> {
        if mm.id() != self.mm_id || authorization.mm_id() != self.mm_id {
            return None;
        }
        let mut state = self.state.lock_irqsave();
        if state.status != ReapStatus::Idle {
            return None;
        }
        let mut job = state.spare.take().expect("idle reaper slot has a job");
        job.expected_gen = mm.tlb_gen.load(Ordering::SeqCst);
        job.mm = Some(mm.clone());
        job.authorization = Some(authorization);
        job.not_before_ns = committed_at_ns.saturating_add(EXIT_GRACE_NS);
        state.status = ReapStatus::Queued;
        Some(job)
    }

    /// Shared-global-init authorization must be resolved before publication.
    /// An already executing job cannot retrospectively establish that safety.
    /// Keep an unclaimed spare: dropping its budget here could retire objects
    /// under the PID membership lock used by the authorization transaction.
    pub(crate) fn deny_before_execution(&self) -> bool {
        let mut state = self.state.lock_irqsave();
        match state.status {
            ReapStatus::Idle | ReapStatus::Queued => {
                state.status = ReapStatus::Finished(ReapFinish::Protected);
                true
            }
            ReapStatus::Finished(ReapFinish::Protected) => true,
            _ => false,
        }
    }

    fn finish(&self, reason: ReapFinish) -> ReapStep {
        self.state.lock_irqsave().status = ReapStatus::Finished(reason);
        ReapStep::Terminal(reason)
    }
}

pub(crate) struct OomReapJob {
    mm: Option<Arc<AddressSpace>>,
    authorization: Option<OomReapAuthorization>,
    next: Option<OwnedBox<OomReapJob>>,
    cursor: VirtAddr,
    expected_gen: u64,
    collisions: u8,
    foreign_restarts: u8,
    not_before_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReapStep {
    /// Caller yields outside all resource locks before taking the next batch.
    Continue,
    /// Use the worker's preallocated deadline event; not a new sleep timer.
    Delay { deadline_ns: u64 },
    /// Drop the owned job/old MM outside queue, slot and MM resource locks.
    Terminal(ReapFinish),
}

impl OomReapJob {
    /// Transfer the original commit permission once, without recreating it.
    pub(crate) fn take_authorization(&mut self) -> OomReapAuthorization {
        self.authorization
            .take()
            .expect("claimed job has committed permission")
    }

    pub(crate) fn mm_id(&self) -> u64 {
        self.mm.as_ref().expect("claimed job has an MM").id()
    }

    pub(crate) fn run_step(
        &mut self,
        slot: &OomReapSlot,
        authorization: &OomReapAuthorization,
        now_ns: u64,
    ) -> ReapStep {
        // Incorrect slot use must not change another MM's state.
        assert_eq!(slot.mm_id, self.mm_id());
        if let ReapStatus::Finished(reason) = slot.status() {
            return ReapStep::Terminal(reason);
        }
        if self.mm.as_ref().is_some_and(|mm| mm.is_torn_down()) {
            return slot.finish(ReapFinish::Scanned);
        }
        if now_ns < self.not_before_ns {
            return ReapStep::Delay {
                deadline_ns: self.not_before_ns,
            };
        }
        {
            let mut state = slot.state.lock_irqsave();
            match state.status {
                ReapStatus::Queued => state.status = ReapStatus::Reaping,
                ReapStatus::Reaping => {}
                ReapStatus::Finished(reason) => return ReapStep::Terminal(reason),
                ReapStatus::Idle => panic!("unclaimed reaper execution"),
            }
        }
        // The last collided attempt is not re-executed. Returning a deadline
        // for each of ten failures matches Linux's ten bounded idle delays.
        if self.collisions == MAX_COLLISIONS {
            return slot.finish(ReapFinish::CollisionLimit);
        }
        let mm = self.mm.as_ref().expect("claimed job has an MM");
        match mm.try_oom_reap_batch(Some(authorization), self.cursor, self.expected_gen) {
            OomReapBatchResult::Continue {
                next, expected_gen, ..
            } => {
                self.cursor = next;
                self.expected_gen = expected_gen;
                ReapStep::Continue
            }
            OomReapBatchResult::Busy {
                resume,
                expected_gen,
                ..
            } => {
                self.cursor = resume;
                self.expected_gen = expected_gen;
                self.collisions += 1;
                ReapStep::Delay {
                    deadline_ns: now_ns.saturating_add(COLLISION_DELAY_NS),
                }
            }
            OomReapBatchResult::Restart { expected_gen, .. } => {
                self.foreign_restarts += 1;
                if self.foreign_restarts == MAX_FOREIGN_RESTARTS {
                    return slot.finish(ReapFinish::ForeignMutationLimit);
                }
                self.cursor = VirtAddr::new(0);
                self.expected_gen = expected_gen;
                ReapStep::Continue
            }
            OomReapBatchResult::Incomplete { .. } => slot.finish(ReapFinish::UnsupportedMapping),
            OomReapBatchResult::PassEnd => slot.finish(ReapFinish::Scanned),
            OomReapBatchResult::Protected => slot.finish(ReapFinish::Protected),
        }
    }
}

/// A dedicated owned chain, not a reusable workqueue or an MM registry.
pub(crate) struct OomReapQueue {
    head: SpinLock<Option<OwnedBox<OomReapJob>>>,
}

impl OomReapQueue {
    pub(crate) const fn new() -> Self {
        Self {
            head: SpinLock::new(None),
        }
    }

    pub(crate) fn push(&self, mut job: OwnedBox<OomReapJob>) {
        assert!(job.next.is_none());
        let mut head = self.head.lock_irqsave();
        job.next = head.take();
        *head = Some(job);
    }

    pub(crate) fn pop(&self) -> Option<OwnedBox<OomReapJob>> {
        let mut head = self.head.lock_irqsave();
        let mut job = head.take()?;
        *head = job.next.take();
        Some(job)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.head.lock_irqsave().is_none()
    }

    /// Detach a finite batch. New arrivals cannot continually jump ahead of
    /// the old batch; all node/MM destruction remains outside the queue lock.
    fn take_batch(&self) -> Option<OwnedBox<OomReapJob>> {
        self.head.lock_irqsave().take()
    }
}

impl Drop for OomReapQueue {
    fn drop(&mut self) {
        let mut next = self.head.lock_irqsave().take();
        while let Some(mut job) = next {
            next = job.next.take();
            // Both the chain and its MM pins retire without the queue lock.
            drop(job);
        }
    }
}
