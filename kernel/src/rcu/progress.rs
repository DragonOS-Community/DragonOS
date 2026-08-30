//! Deterministic grace-period escalation policy.
//!
//! This module owns no locks, clocks, IPI delivery, or logging. The RCU
//! coordinator supplies a timestamp and the authoritative holdout mask, then
//! executes the returned actions after dropping its lock.

use crate::{libs::cpumask::CpuMask, mm::percpu::PerCpu, smp::cpu::ProcessorId};

use super::gp::RcuSequence;

pub(super) const SOFT_REQUEST_NS: u64 = 100_000_000;
pub(super) const RESCHED_DELAY_NS: u64 = 1_000_000_000;
pub(super) const RESCHED_REPEAT_NS: u64 = 1_000_000_000;
pub(super) const STALL_TIMEOUT_NS: u64 = 21_000_000_000;
pub(super) const STALL_REPEAT_NS: u64 = 63_000_000_000;
const STALL_RETRY_NS: u64 = 1_000_000_000;

const HALF_RANGE: u64 = 1_u64 << 63;
const MASK_WORDS: usize = (PerCpu::MAX_CPU_NUM as usize).div_ceil(u64::BITS as usize);

/// Allocation-free holdout snapshot used by timer-interrupt escalation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ProgressCpuMask {
    words: [u64; MASK_WORDS],
}

impl ProgressCpuMask {
    pub(super) const fn new() -> Self {
        Self {
            words: [0; MASK_WORDS],
        }
    }

    pub(super) fn from_cpu_mask(mask: &CpuMask) -> Self {
        let mut result = Self::new();
        for cpu in mask.iter_cpu() {
            result.set(cpu, true);
        }
        result
    }

    fn from_cpu(cpu: ProcessorId) -> Self {
        let mut result = Self::new();
        result.set(cpu, true);
        result
    }

    pub(super) fn set(&mut self, cpu: ProcessorId, value: bool) {
        let index = cpu.data() as usize;
        debug_assert!(index < PerCpu::MAX_CPU_NUM as usize);
        let word = index / u64::BITS as usize;
        let bit = 1u64 << (index % u64::BITS as usize);
        if value {
            self.words[word] |= bit;
        } else {
            self.words[word] &= !bit;
        }
    }

    fn get(&self, cpu: ProcessorId) -> bool {
        let index = cpu.data() as usize;
        let word = index / u64::BITS as usize;
        let bit = 1u64 << (index % u64::BITS as usize);
        self.words[word] & bit != 0
    }

    pub(super) fn is_empty(&self) -> bool {
        self.words.iter().all(|word| *word == 0)
    }

    fn intersect(&mut self, other: &Self) {
        for (word, other_word) in self.words.iter_mut().zip(other.words.iter()) {
            *word &= *other_word;
        }
    }

    pub(super) fn iter_cpu(&self) -> ProgressCpuMaskIter<'_> {
        ProgressCpuMaskIter {
            mask: self,
            next: 0,
        }
    }
}

pub(super) struct ProgressCpuMaskIter<'a> {
    mask: &'a ProgressCpuMask,
    next: u32,
}

impl Iterator for ProgressCpuMaskIter<'_> {
    type Item = ProcessorId;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next < PerCpu::MAX_CPU_NUM {
            let cpu = ProcessorId::new(self.next);
            self.next += 1;
            if self.mask.get(cpu) {
                return Some(cpu);
            }
        }
        None
    }
}

#[inline]
pub(super) const fn elapsed_at_least(now: u64, origin: u64, interval: u64) -> bool {
    now.wrapping_sub(origin) >= interval && now.wrapping_sub(origin) < HALF_RANGE
}

#[inline]
pub(super) const fn deadline_reached(now: u64, deadline: u64) -> bool {
    now.wrapping_sub(deadline) < HALF_RANGE
}

#[derive(Clone, Copy)]
pub(super) struct StallCandidate {
    pub(super) seq: RcuSequence,
    pub(super) started_at: u64,
    pub(super) last_progress_at: u64,
    pub(super) holdouts: ProgressCpuMask,
    pub(super) report_number: u64,
}

pub(super) struct ProgressActions {
    pub(super) soft: ProgressCpuMask,
    pub(super) resched: ProgressCpuMask,
    pub(super) stall: Option<StallCandidate>,
    pub(super) next_deadline: u64,
}

pub(super) struct ActiveGpProgress {
    seq: RcuSequence,
    started_at: u64,
    last_progress_at: u64,
    soft_requested: bool,
    last_resched_at: Option<u64>,
    last_stall_attempt_at: Option<u64>,
    last_stall_report_at: Option<u64>,
    stall_reports: u64,
}

impl ActiveGpProgress {
    pub(super) fn new(seq: RcuSequence, now: u64) -> Self {
        Self {
            seq,
            started_at: now,
            last_progress_at: now,
            soft_requested: false,
            last_resched_at: None,
            last_stall_attempt_at: None,
            last_stall_report_at: None,
            stall_reports: 0,
        }
    }

    #[inline]
    pub(super) fn seq(&self) -> RcuSequence {
        self.seq
    }

    #[inline]
    pub(super) fn started_at(&self) -> u64 {
        self.started_at
    }

    #[inline]
    pub(super) fn last_progress_at(&self) -> u64 {
        self.last_progress_at
    }

    #[inline]
    pub(super) fn note_progress(&mut self, now: u64) {
        self.last_progress_at = now;
    }

    pub(super) fn next_deadline(&self, now: u64) -> u64 {
        let soft_remaining = if self.soft_requested {
            u64::MAX
        } else {
            remaining(now, self.started_at, SOFT_REQUEST_NS)
        };
        let resched_remaining = match self.last_resched_at {
            Some(last) => remaining(now, last, RESCHED_REPEAT_NS),
            None => remaining(now, self.started_at, RESCHED_DELAY_NS),
        };
        let report_remaining = match self.last_stall_report_at {
            Some(last) => remaining(now, last, STALL_REPEAT_NS),
            None => remaining(now, self.started_at, STALL_TIMEOUT_NS),
        };
        let retry_remaining = self
            .last_stall_attempt_at
            .map(|last| remaining(now, last, STALL_RETRY_NS))
            .unwrap_or(0);
        let stall_remaining = report_remaining.max(retry_remaining);
        now.wrapping_add(soft_remaining.min(resched_remaining).min(stall_remaining))
    }

    pub(super) fn actions(&mut self, now: u64, holdouts: ProgressCpuMask) -> ProgressActions {
        let soft_due =
            !self.soft_requested && elapsed_at_least(now, self.started_at, SOFT_REQUEST_NS);
        if soft_due {
            self.soft_requested = true;
        }

        let resched_due = match self.last_resched_at {
            Some(last) => elapsed_at_least(now, last, RESCHED_REPEAT_NS),
            None => elapsed_at_least(now, self.started_at, RESCHED_DELAY_NS),
        };
        if resched_due {
            self.last_resched_at = Some(now);
        }

        let report_due = match self.last_stall_report_at {
            Some(last) => elapsed_at_least(now, last, STALL_REPEAT_NS),
            None => elapsed_at_least(now, self.started_at, STALL_TIMEOUT_NS),
        };
        let retry_due = self
            .last_stall_attempt_at
            .map(|last| elapsed_at_least(now, last, STALL_RETRY_NS))
            .unwrap_or(true);
        let stall_due = report_due && retry_due;
        let stall = if stall_due {
            self.last_stall_attempt_at = Some(now);
            Some(StallCandidate {
                seq: self.seq,
                started_at: self.started_at,
                last_progress_at: self.last_progress_at,
                holdouts,
                report_number: self.stall_reports.wrapping_add(1),
            })
        } else {
            None
        };

        ProgressActions {
            soft: if soft_due {
                holdouts
            } else {
                ProgressCpuMask::new()
            },
            resched: if resched_due {
                holdouts
            } else {
                ProgressCpuMask::new()
            },
            stall,
            next_deadline: self.next_deadline(now),
        }
    }

    /// Commits rate limiting only after the reporter has revalidated the GP.
    pub(super) fn commit_stall_report(
        &mut self,
        seq: RcuSequence,
        report_number: u64,
        now: u64,
    ) -> bool {
        if self.seq != seq || report_number != self.stall_reports.wrapping_add(1) {
            return false;
        }
        self.stall_reports = report_number;
        self.last_stall_report_at = Some(now);
        self.last_stall_attempt_at = None;
        true
    }
}

/// Revalidates and commits a stall candidate against the coordinator's
/// current fixed-size snapshot. Production and deterministic tests share this
/// transition so a stale or already-satisfied report cannot consume the
/// report-rate-limit state.
pub(super) fn validate_and_commit_stall(
    current_seq: RcuSequence,
    current_holdouts: ProgressCpuMask,
    progress: &mut ActiveGpProgress,
    mut candidate: StallCandidate,
    now: u64,
) -> Option<StallCandidate> {
    if candidate.seq != current_seq || progress.seq() != current_seq {
        return None;
    }
    candidate.holdouts.intersect(&current_holdouts);
    if candidate.holdouts.is_empty() {
        return None;
    }
    candidate.last_progress_at = progress.last_progress_at();
    progress
        .commit_stall_report(candidate.seq, candidate.report_number, now)
        .then_some(candidate)
}

#[inline]
fn remaining(now: u64, origin: u64, interval: u64) -> u64 {
    let elapsed = now.wrapping_sub(origin);
    if elapsed >= interval && elapsed < HALF_RANGE {
        0
    } else {
        interval.wrapping_sub(elapsed)
    }
}

pub(super) fn run_progress_selftests() -> Result<(), &'static str> {
    run_escalation_selftest()?;
    run_stall_repeat_selftest()?;
    run_partial_progress_selftest()?;
    run_deadline_wrap_selftest()?;
    run_stall_validation_selftest()?;
    Ok(())
}

#[inline(never)]
fn run_stall_validation_selftest() -> Result<(), &'static str> {
    let seq = RcuSequence::from_raw(11);
    let old_seq = RcuSequence::from_raw(9);
    let cpu0 = ProcessorId::new(0);
    let cpu1 = ProcessorId::new(1);
    let mut original = ProgressCpuMask::from_cpu(cpu0);
    original.set(cpu1, true);

    let mut stale_progress = ActiveGpProgress::new(seq, 0);
    let stale = stale_progress
        .actions(STALL_TIMEOUT_NS, original)
        .stall
        .unwrap();
    if validate_and_commit_stall(
        old_seq,
        original,
        &mut stale_progress,
        stale,
        STALL_TIMEOUT_NS,
    )
    .is_some()
    {
        return Err("RCU stall validation accepted a stale GP sequence");
    }

    let mut empty_progress = ActiveGpProgress::new(seq, 0);
    let empty = empty_progress
        .actions(STALL_TIMEOUT_NS, original)
        .stall
        .unwrap();
    if validate_and_commit_stall(
        seq,
        ProgressCpuMask::new(),
        &mut empty_progress,
        empty,
        STALL_TIMEOUT_NS,
    )
    .is_some()
    {
        return Err("RCU stall validation accepted an empty holdout set");
    }

    let mut valid_progress = ActiveGpProgress::new(seq, 0);
    valid_progress.note_progress(STALL_TIMEOUT_NS - 1);
    let valid = valid_progress
        .actions(STALL_TIMEOUT_NS, original)
        .stall
        .unwrap();
    let current = ProgressCpuMask::from_cpu(cpu1);
    let committed =
        validate_and_commit_stall(seq, current, &mut valid_progress, valid, STALL_TIMEOUT_NS)
            .ok_or("RCU stall validation rejected a current candidate")?;
    if committed.holdouts != current
        || committed.last_progress_at != STALL_TIMEOUT_NS - 1
        || committed.report_number != 1
    {
        return Err("RCU stall validation did not refresh and filter its candidate");
    }
    if valid_progress
        .actions(STALL_TIMEOUT_NS + STALL_REPEAT_NS - 1, current)
        .stall
        .is_some()
    {
        return Err("RCU stall validation did not commit report rate limiting");
    }
    Ok(())
}

#[inline(never)]
fn run_deadline_wrap_selftest() -> Result<(), &'static str> {
    let seq = RcuSequence::from_raw(9);
    let started_at = 0u64.wrapping_sub(SOFT_REQUEST_NS);
    let holdout = ProgressCpuMask::from_cpu(ProcessorId::new(0));
    let mut progress = ActiveGpProgress::new(seq, started_at);
    if progress.next_deadline(started_at) != 0 {
        return Err("RCU progress selftest did not construct a zero wrapping deadline");
    }
    let actions = progress.actions(0, holdout);
    if actions.soft != holdout {
        return Err("RCU progress policy lost a valid zero wrapping deadline");
    }
    Ok(())
}

#[inline(never)]
fn run_escalation_selftest() -> Result<(), &'static str> {
    let seq = RcuSequence::from_raw(7);
    let mut holdouts = ProgressCpuMask::from_cpu(ProcessorId::new(0));
    holdouts.set(ProcessorId::new(1), true);
    let mut progress = ActiveGpProgress::new(seq, 100);

    let before = progress.actions(100 + SOFT_REQUEST_NS - 1, holdouts);
    if !before.soft.is_empty() || !before.resched.is_empty() || before.stall.is_some() {
        return Err("RCU progress policy escalated before its deadline");
    }
    let soft = progress.actions(100 + SOFT_REQUEST_NS, holdouts);
    if soft.soft != holdouts || !soft.resched.is_empty() || soft.stall.is_some() {
        return Err("RCU progress policy did not issue the soft request exactly on time");
    }
    let hard = progress.actions(100 + RESCHED_DELAY_NS, holdouts);
    if hard.resched != holdouts || hard.stall.is_some() {
        return Err("RCU progress policy did not issue the reschedule action");
    }
    Ok(())
}

#[inline(never)]
fn run_stall_repeat_selftest() -> Result<(), &'static str> {
    // Force both independent actions due in one evaluation. A mutually
    // exclusive implementation would silently starve the stall report.
    let seq = RcuSequence::from_raw(7);
    let mut holdouts = ProgressCpuMask::from_cpu(ProcessorId::new(0));
    holdouts.set(ProcessorId::new(1), true);
    let mut simultaneous = ActiveGpProgress::new(seq, 0);
    simultaneous.last_resched_at = Some(STALL_TIMEOUT_NS - RESCHED_REPEAT_NS);
    let both = simultaneous.actions(STALL_TIMEOUT_NS, holdouts);
    if both.resched != holdouts || both.stall.is_none() {
        return Err("RCU hard escalation starved a simultaneous stall report");
    }
    let report_number = both.stall.as_ref().unwrap().report_number;
    if !simultaneous.commit_stall_report(seq, report_number, STALL_TIMEOUT_NS) {
        return Err("RCU progress policy rejected a valid stall report commit");
    }
    let before_repeat = simultaneous.actions(STALL_TIMEOUT_NS + STALL_REPEAT_NS - 1, holdouts);
    if before_repeat.stall.is_some() {
        return Err("RCU progress policy repeated a stall report too early");
    }
    let repeat = simultaneous.actions(STALL_TIMEOUT_NS + STALL_REPEAT_NS, holdouts);
    if repeat
        .stall
        .as_ref()
        .map(|candidate| candidate.report_number)
        != Some(2)
    {
        return Err("RCU progress policy did not repeat a persistent stall");
    }

    let mut retry = ActiveGpProgress::new(seq, 0);
    let first_attempt = retry.actions(STALL_TIMEOUT_NS, holdouts);
    let expected_report = first_attempt.stall.as_ref().unwrap().report_number;
    if retry
        .actions(STALL_TIMEOUT_NS + STALL_RETRY_NS - 1, holdouts)
        .stall
        .is_some()
    {
        return Err("RCU progress policy retried a dropped stall report too early");
    }
    let retried = retry.actions(STALL_TIMEOUT_NS + STALL_RETRY_NS, holdouts);
    if retried
        .stall
        .as_ref()
        .map(|candidate| candidate.report_number)
        != Some(expected_report)
    {
        return Err("RCU progress policy did not retry a dropped stall report");
    }
    Ok(())
}

#[inline(never)]
fn run_partial_progress_selftest() -> Result<(), &'static str> {
    let seq = RcuSequence::from_raw(7);
    let mut progress = ActiveGpProgress::new(seq, 100);
    let initial = ProgressCpuMask::from_cpu(ProcessorId::new(1));
    let _ = progress.actions(100 + RESCHED_DELAY_NS, initial);
    let mut remaining_holdout = ProgressCpuMask::from_cpu(ProcessorId::new(1));
    progress.note_progress(100 + RESCHED_DELAY_NS + 1);
    let repeated = progress.actions(
        100 + RESCHED_DELAY_NS + RESCHED_REPEAT_NS,
        remaining_holdout,
    );
    if repeated.resched != remaining_holdout {
        return Err("RCU progress policy retained a CPU that already reported QS");
    }
    remaining_holdout.set(ProcessorId::new(1), false);
    if !remaining_holdout.is_empty() {
        return Err("RCU progress selftest constructed an invalid empty mask");
    }

    Ok(())
}
