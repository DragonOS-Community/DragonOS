//! Deterministic state transitions for ordinary RCU grace periods.
//!
//! This module deliberately contains no locking, CPU-topology lookup, wakeup,
//! or callback invocation. Callers serialize it with `RcuState::inner` and are
//! responsible for the full memory barriers documented in `rcu/mod.rs`.

use crate::{libs::cpumask::CpuMask, mm::percpu::PerCpu, smp::cpu::ProcessorId};

const SEQUENCE_HALF_RANGE: u64 = 1_u64 << 63;

/// A wrapping RCU progress ticket.
///
/// Comparisons are valid while the distance between an outstanding ticket and
/// the current value is less than half the sequence space. Grace-period
/// requests are at most one generation ahead, and the number of outstanding
/// callbacks is bounded by available memory, so both users satisfy this rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RcuSequence(u64);

impl RcuSequence {
    const ZERO: Self = Self(0);
    const FIRST_CALLBACK: Self = Self(1);

    #[inline]
    pub(super) const fn raw(self) -> u64 {
        self.0
    }

    #[inline]
    pub(super) const fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }

    /// Returns whether `self` is at or after `target` in wrapping order.
    #[inline]
    pub(super) const fn has_reached(self, target: Self) -> bool {
        self.0.wrapping_sub(target.0) < SEQUENCE_HALF_RANGE
    }
}

struct ActiveGracePeriod {
    seq: RcuSequence,
    waiting_cpus: CpuMask,
    context_generations: [u64; PerCpu::MAX_CPU_NUM as usize],
}

/// The minimal ordinary-RCU grace-period state machine.
///
/// `next_requested` coalesces all requests for the one future generation that
/// can be required beyond the active GP. A request made during an active GP
/// must target the following GP because the active GP's CPU snapshot predates
/// that request.
pub(super) struct GracePeriodState {
    completed: RcuSequence,
    active: Option<ActiveGracePeriod>,
    next_requested: bool,
}

impl GracePeriodState {
    pub(super) fn new() -> Self {
        Self::with_completed(RcuSequence::ZERO)
    }

    fn with_completed(completed: RcuSequence) -> Self {
        Self {
            completed,
            active: None,
            next_requested: false,
        }
    }

    /// Requests a GP that starts after this call and returns its completion
    /// ticket. Concurrent requests for the same future generation coalesce.
    pub(super) fn request_future(&mut self) -> RcuSequence {
        self.assert_invariants();
        self.next_requested = true;
        self.active
            .as_ref()
            .map_or_else(|| self.completed.next(), |active| active.seq.next())
    }

    pub(super) fn has_request(&self) -> bool {
        self.next_requested
    }

    pub(super) fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Starts the requested GP with a freshly collected CPU snapshot.
    pub(super) fn start_requested(
        &mut self,
        waiting_cpus: CpuMask,
        context_generations: [u64; PerCpu::MAX_CPU_NUM as usize],
    ) -> RcuSequence {
        self.assert_invariants();
        debug_assert!(
            self.active.is_none(),
            "started an RCU GP while one is active"
        );
        debug_assert!(self.next_requested, "started an unrequested RCU GP");

        let seq = self.completed.next();
        self.next_requested = false;
        self.active = Some(ActiveGracePeriod {
            seq,
            waiting_cpus,
            context_generations,
        });
        self.assert_invariants();
        seq
    }

    pub(super) fn is_waiting_for(&self, cpu: ProcessorId) -> bool {
        self.active
            .as_ref()
            .and_then(|active| active.waiting_cpus.get(cpu))
            .unwrap_or(false)
    }

    /// Clears a CPU from the active GP. Repeated/non-waiting reports are
    /// intentionally idempotent and return false.
    pub(super) fn report_quiescent_state(&mut self, cpu: ProcessorId) -> bool {
        self.assert_invariants();
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        if !active.waiting_cpus.get(cpu).unwrap_or(false) {
            return false;
        }

        active.waiting_cpus.set(cpu, false);
        true
    }

    /// Credits a CPU whose context snapshot proves that it is currently in
    /// an extended quiescent state or has passed through one since GP start.
    pub(super) fn report_context_progress(
        &mut self,
        cpu: ProcessorId,
        current_generation: u64,
        in_eqs: bool,
    ) -> bool {
        self.assert_invariants();
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        if !active.waiting_cpus.get(cpu).unwrap_or(false) {
            return false;
        }

        let start_generation = active.context_generations[cpu.data() as usize];
        if !in_eqs && current_generation == start_generation {
            return false;
        }

        active.waiting_cpus.set(cpu, false);
        true
    }

    pub(super) fn ready_to_complete(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.waiting_cpus.is_empty())
    }

    /// Completes the active GP after the coordinator has executed the GP-end
    /// full barrier.
    pub(super) fn complete_ready(&mut self) -> RcuSequence {
        self.assert_invariants();
        let active = self
            .active
            .take()
            .expect("completed RCU GP while no GP is active");
        debug_assert!(
            active.waiting_cpus.is_empty(),
            "completed RCU GP with waiting CPUs"
        );
        debug_assert_eq!(active.seq, self.completed.next());
        self.completed = active.seq;
        self.assert_invariants();
        self.completed
    }

    pub(super) fn has_completed(&self, target: RcuSequence) -> bool {
        self.completed.has_reached(target)
    }

    pub(super) fn current(&self) -> RcuSequence {
        self.active
            .as_ref()
            .map_or(self.completed, |active| active.seq)
    }

    pub(super) fn completed(&self) -> RcuSequence {
        self.completed
    }

    fn assert_invariants(&self) {
        if let Some(active) = &self.active {
            debug_assert_eq!(
                active.seq,
                self.completed.next(),
                "active RCU GP must immediately follow the completed generation"
            );
        }
    }
}

/// Callback admission/completion tickets plus unique drainer ownership.
///
/// Callback storage remains in `RcuStateInner`; this type only centralizes the
/// ordering facts required by `rcu_barrier()`.
pub(super) struct CallbackTracker {
    next: RcuSequence,
    last_admitted: Option<RcuSequence>,
    completed: Option<RcuSequence>,
    draining: bool,
}

impl CallbackTracker {
    pub(super) fn new() -> Self {
        Self::with_next(RcuSequence::FIRST_CALLBACK)
    }

    fn with_next(next: RcuSequence) -> Self {
        Self {
            next,
            last_admitted: None,
            completed: None,
            draining: false,
        }
    }

    pub(super) fn admit(&mut self) -> RcuSequence {
        let seq = self.next;
        self.next = self.next.next();
        self.last_admitted = Some(seq);
        seq
    }

    pub(super) fn barrier_target(&self) -> Option<RcuSequence> {
        self.last_admitted
    }

    pub(super) fn has_completed(&self, target: RcuSequence) -> bool {
        self.completed
            .is_some_and(|completed| completed.has_reached(target))
    }

    pub(super) fn complete(&mut self, seq: RcuSequence) {
        debug_assert!(
            self.last_admitted.is_some_and(|last| last.has_reached(seq)),
            "completed a callback that was not admitted"
        );
        if let Some(completed) = self.completed {
            debug_assert_eq!(
                seq,
                completed.next(),
                "RCU callbacks must complete in admission order"
            );
        }
        self.completed = Some(seq);
    }

    pub(super) fn try_claim_drainer(&mut self) -> bool {
        if self.draining {
            return false;
        }
        self.draining = true;
        true
    }

    pub(super) fn release_drainer(&mut self) {
        debug_assert!(self.draining, "released an unclaimed RCU callback drainer");
        self.draining = false;
    }

    pub(super) fn drainer_available(&self) -> bool {
        !self.draining
    }

    pub(super) fn completed_raw(&self) -> u64 {
        self.completed.map_or(0, RcuSequence::raw)
    }
}

fn one_cpu_mask(cpu: u32) -> CpuMask {
    CpuMask::from_cpu(ProcessorId::new(cpu))
}

fn zero_context_generations() -> [u64; PerCpu::MAX_CPU_NUM as usize] {
    [0; PerCpu::MAX_CPU_NUM as usize]
}

/// Runs deterministic tests against the production state-transition methods.
pub(super) fn run_state_machine_selftests() -> Result<(), &'static str> {
    let cpu0 = ProcessorId::new(0);
    let cpu1 = ProcessorId::new(1);

    let mut gp = GracePeriodState::new();
    let first = gp.request_future();
    if gp.request_future() != first {
        return Err("idle RCU GP requests did not coalesce");
    }
    if gp.start_requested(one_cpu_mask(0), zero_context_generations()) != first
        || !gp.is_waiting_for(cpu0)
    {
        return Err("RCU GP did not start with the requested waiting mask");
    }
    let second = gp.request_future();
    if second != first.next() || gp.request_future() != second {
        return Err("active RCU GP requests did not coalesce into the next generation");
    }
    if !gp.report_quiescent_state(cpu0) || gp.report_quiescent_state(cpu0) {
        return Err("RCU GP quiescent-state reporting was not idempotent");
    }
    if gp.complete_ready() != first || gp.has_completed(second) {
        return Err("RCU GP completed a nested future request too early");
    }
    if gp.start_requested(one_cpu_mask(1), zero_context_generations()) != second
        || !gp.is_waiting_for(cpu1)
    {
        return Err("consecutive RCU GP did not use its fresh waiting mask");
    }
    if !gp.report_quiescent_state(cpu1) || gp.complete_ready() != second {
        return Err("consecutive RCU GP did not complete in sequence");
    }

    let mut context_gp = GracePeriodState::new();
    let context_target = context_gp.request_future();
    let mut context_generations = zero_context_generations();
    context_generations[cpu0.data() as usize] = 7;
    context_generations[cpu1.data() as usize] = 11;
    let mut context_waiting = one_cpu_mask(0);
    context_waiting.set(cpu1, true);
    context_gp.start_requested(context_waiting, context_generations);
    if context_gp.report_context_progress(cpu0, 7, false) || !context_gp.is_waiting_for(cpu0) {
        return Err("RCU GP credited a CPU without context progress");
    }
    if !context_gp.report_context_progress(cpu0, 8, false)
        || !context_gp.report_context_progress(cpu1, 11, true)
        || !context_gp.ready_to_complete()
        || context_gp.complete_ready() != context_target
    {
        return Err("RCU GP did not credit generation change and current EQS");
    }

    let mut empty_gp = GracePeriodState::new();
    let empty_target = empty_gp.request_future();
    empty_gp.start_requested(CpuMask::new(), zero_context_generations());
    if !empty_gp.ready_to_complete()
        || empty_gp.complete_ready() != empty_target
        || empty_gp.is_active()
    {
        return Err("RCU GP with an empty waiting mask did not complete immediately");
    }

    let mut wrapping_gp = GracePeriodState::with_completed(RcuSequence(u64::MAX.wrapping_sub(1)));
    let max_target = wrapping_gp.request_future();
    wrapping_gp.start_requested(CpuMask::new(), zero_context_generations());
    if max_target.raw() != u64::MAX || wrapping_gp.complete_ready() != max_target {
        return Err("RCU GP did not advance to u64::MAX");
    }
    let zero_target = wrapping_gp.request_future();
    wrapping_gp.start_requested(CpuMask::new(), zero_context_generations());
    if zero_target.raw() != 0
        || wrapping_gp.complete_ready() != zero_target
        || !wrapping_gp.has_completed(max_target)
        || !wrapping_gp.has_completed(zero_target)
    {
        return Err("RCU GP sequence did not wrap from u64::MAX to zero");
    }

    let mut callbacks = CallbackTracker::with_next(RcuSequence(u64::MAX));
    let max_callback = callbacks.admit();
    if callbacks.barrier_target() != Some(max_callback) || callbacks.has_completed(max_callback) {
        return Err("RCU callback tracker corrupted its pre-wrap barrier target");
    }
    callbacks.complete(max_callback);
    let zero_callback = callbacks.admit();
    if zero_callback.raw() != 0 || callbacks.barrier_target() != Some(zero_callback) {
        return Err("RCU callback tracker treated wrapped zero as an empty target");
    }
    callbacks.complete(zero_callback);
    if !callbacks.has_completed(max_callback) || !callbacks.has_completed(zero_callback) {
        return Err("RCU callback completion comparison failed across sequence wrap");
    }
    if !callbacks.try_claim_drainer() || callbacks.try_claim_drainer() {
        return Err("RCU callback tracker allowed two simultaneous drainers");
    }
    callbacks.release_drainer();
    if !callbacks.try_claim_drainer() {
        return Err("RCU callback tracker did not release drainer ownership");
    }
    callbacks.release_drainer();

    Ok(())
}
