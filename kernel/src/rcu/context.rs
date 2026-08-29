//! Atomic per-CPU context tracking for ordinary RCU.
//!
//! This module owns only the local context state machine. It deliberately does
//! not inspect CPU topology or grace-period state, acquire locks, wake tasks,
//! or report diagnostics. The coordinator in `rcu::mod` supplies those policy
//! decisions around these checked, allocation-free transitions.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const BASE_BITS: u32 = 2;
const DEPTH_BITS: u32 = 8;
const BASE_MASK: u64 = (1 << BASE_BITS) - 1;
const IRQ_SHIFT: u32 = BASE_BITS;
const NMI_SHIFT: u32 = IRQ_SHIFT + DEPTH_BITS;
const DEPTH_MASK: u64 = (1 << DEPTH_BITS) - 1;
const IDLE_ACTIVE_BIT: u64 = 1 << (NMI_SHIFT + DEPTH_BITS);
const IDLE_CONSUMED_BIT: u64 = IDLE_ACTIVE_BIT << 1;
const GENERATION_SHIFT: u32 = NMI_SHIFT + DEPTH_BITS + 2;
const GENERATION_BITS: u32 = 64 - GENERATION_SHIFT;
const GENERATION_MASK: u64 = (1_u64 << GENERATION_BITS) - 1;
const MAX_DEPTH: u8 = DEPTH_MASK as u8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BaseContext {
    Kernel = 0,
    User = 1,
    Idle = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ContextSnapshot(u64);

impl ContextSnapshot {
    #[inline]
    pub(super) const fn base(self) -> BaseContext {
        match self.0 & BASE_MASK {
            0 => BaseContext::Kernel,
            1 => BaseContext::User,
            2 => BaseContext::Idle,
            // Only this module constructs state words, and every transition
            // preserves or writes one of the three values above.
            _ => unreachable!(),
        }
    }

    #[inline]
    pub(super) const fn irq_depth(self) -> u8 {
        ((self.0 >> IRQ_SHIFT) & DEPTH_MASK) as u8
    }

    #[inline]
    pub(super) const fn nmi_depth(self) -> u8 {
        ((self.0 >> NMI_SHIFT) & DEPTH_MASK) as u8
    }

    #[inline]
    pub(super) const fn generation(self) -> u64 {
        (self.0 >> GENERATION_SHIFT) & GENERATION_MASK
    }

    #[inline]
    pub(super) const fn is_watching(self) -> bool {
        matches!(self.base(), BaseContext::Kernel) || self.irq_depth() != 0 || self.nmi_depth() != 0
    }

    #[inline]
    pub(super) const fn in_eqs(self) -> bool {
        !self.is_watching()
    }

    #[inline]
    const fn idle_wait_active(self) -> bool {
        self.0 & IDLE_ACTIVE_BIT != 0
    }

    #[inline]
    const fn idle_wait_consumed(self) -> bool {
        self.0 & IDLE_CONSUMED_BIT != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ContextTransition {
    before: ContextSnapshot,
    after: ContextSnapshot,
}

impl ContextTransition {
    #[inline]
    pub(super) const fn before(self) -> ContextSnapshot {
        self.before
    }

    #[inline]
    pub(super) const fn after(self) -> ContextSnapshot {
        self.after
    }

    #[inline]
    pub(super) const fn entered_eqs(self) -> bool {
        self.before.is_watching() && self.after.in_eqs()
    }

    #[inline]
    pub(super) const fn exited_eqs(self) -> bool {
        self.before.in_eqs() && self.after.is_watching()
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct IdleTransition(ContextTransition);

impl IdleTransition {
    #[inline]
    pub(super) const fn transition(&self) -> ContextTransition {
        self.0
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct IrqEntry {
    depth: u8,
    outermost: bool,
    transition: ContextTransition,
}

impl IrqEntry {
    #[inline]
    pub(super) const fn depth(&self) -> u8 {
        self.depth
    }

    #[inline]
    pub(super) const fn outermost(&self) -> bool {
        self.outermost
    }

    #[inline]
    pub(super) const fn transition(&self) -> ContextTransition {
        self.transition
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct NmiEntry {
    depth: u8,
    transition: ContextTransition,
}

impl NmiEntry {
    #[inline]
    pub(super) const fn depth(&self) -> u8 {
        self.depth
    }

    #[inline]
    pub(super) const fn transition(&self) -> ContextTransition {
        self.transition
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum IrqDisposition {
    ResumeInterrupted,
    ToKernel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ContextOperation {
    UserEnter,
    UserExit,
    IdleEnter,
    IdleExit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ContextTransitionError {
    WrongBase {
        operation: ContextOperation,
        expected: BaseContext,
        actual: BaseContext,
    },
    OverlayActive {
        operation: ContextOperation,
        irq_depth: u8,
        nmi_depth: u8,
    },
    IdleWaitAlreadyActive,
    IdleWaitMissing,
    IdleWaitProtocolCorrupt,
    IrqDepthOverflow,
    IrqDepthUnderflow,
    IrqEntryMismatch {
        expected_depth: u8,
        actual_depth: u8,
    },
    ToKernelRequiresOutermost,
    ToKernelDuringNmi,
    NmiDepthOverflow,
    NmiDepthUnderflow,
    NmiEntryMismatch {
        expected_depth: u8,
        actual_depth: u8,
    },
}

/// A cache-line-isolated tracker. Instances are intended to be stored per CPU.
#[repr(align(64))]
pub(super) struct RcuContextTracker {
    state: AtomicU64,
    /// Coordinator-owned filter for the GP slow path. The GP waiting mask,
    /// not this hint, remains the source of truth for progress.
    gp_report_needed: AtomicBool,
}

impl RcuContextTracker {
    pub(super) const fn new() -> Self {
        Self {
            state: AtomicU64::new(BaseContext::Kernel as u64),
            gp_report_needed: AtomicBool::new(false),
        }
    }

    /// Resets an AP's context before it becomes eligible for future GPs.
    ///
    /// The hotplug coordinator must exclude this CPU from both the RCU
    /// participation mask and the active GP while this runs. There can be no
    /// local context transition because the AP has not started executing yet.
    pub(super) fn reset_for_cpu_starting(&self) {
        self.gp_report_needed.store(false, Ordering::SeqCst);
        self.state
            .store(BaseContext::Kernel as u64, Ordering::SeqCst);
    }

    /// Must be published before the coordinator snapshots this CPU.
    #[inline]
    pub(super) fn prepare_gp_report(&self) {
        self.gp_report_needed.store(true, Ordering::SeqCst);
    }

    #[inline]
    pub(super) fn clear_gp_report(&self) {
        self.gp_report_needed.store(false, Ordering::SeqCst);
    }

    #[inline]
    pub(super) fn gp_report_needed(&self) -> bool {
        self.gp_report_needed.load(Ordering::SeqCst)
    }

    /// Returns one coherent view of all context fields.
    #[inline]
    pub(super) fn snapshot(&self) -> ContextSnapshot {
        ContextSnapshot(self.state.load(Ordering::SeqCst))
    }

    pub(super) fn user_enter(&self) -> Result<ContextTransition, ContextTransitionError> {
        self.update(|before| {
            require_no_overlay(before, ContextOperation::UserEnter)?;
            require_base(before, ContextOperation::UserEnter, BaseContext::Kernel)?;
            Ok(with_base(before.0, BaseContext::User))
        })
    }

    pub(super) fn user_exit(&self) -> Result<ContextTransition, ContextTransitionError> {
        self.update(|before| {
            require_no_overlay(before, ContextOperation::UserExit)?;
            require_base(before, ContextOperation::UserExit, BaseContext::User)?;
            Ok(with_base(before.0, BaseContext::Kernel))
        })
    }

    pub(super) fn idle_enter(&self) -> Result<IdleTransition, ContextTransitionError> {
        self.update(|before| {
            require_no_overlay(before, ContextOperation::IdleEnter)?;
            if before.idle_wait_active() {
                return Err(ContextTransitionError::IdleWaitAlreadyActive);
            }
            if before.idle_wait_consumed() {
                return Err(ContextTransitionError::IdleWaitProtocolCorrupt);
            }
            require_base(before, ContextOperation::IdleEnter, BaseContext::Kernel)?;

            let next = with_base(before.0, BaseContext::Idle) | IDLE_ACTIVE_BIT;
            Ok(next & !IDLE_CONSUMED_BIT)
        })
        .map(IdleTransition)
    }

    /// Completes the one outstanding idle wait.
    ///
    /// An IRQ `ToKernel` transition may already have consumed the EQS. In that
    /// case this clears only the persistent wait protocol bits. Otherwise this
    /// performs the normal Idle-to-Kernel transition.
    pub(super) fn idle_exit(&self) -> Result<ContextTransition, ContextTransitionError> {
        self.update(|before| {
            require_no_overlay(before, ContextOperation::IdleExit)?;
            if !before.idle_wait_active() {
                return Err(ContextTransitionError::IdleWaitMissing);
            }

            if before.idle_wait_consumed() {
                require_base(before, ContextOperation::IdleExit, BaseContext::Kernel)?;
                Ok(before.0 & !(IDLE_ACTIVE_BIT | IDLE_CONSUMED_BIT))
            } else {
                require_base(before, ContextOperation::IdleExit, BaseContext::Idle)?;
                Ok(with_base(before.0, BaseContext::Kernel)
                    & !(IDLE_ACTIVE_BIT | IDLE_CONSUMED_BIT))
            }
        })
    }

    pub(super) fn irq_enter(&self) -> Result<IrqEntry, ContextTransitionError> {
        let transition = self.update(|before| {
            let depth = before.irq_depth();
            if depth == MAX_DEPTH {
                return Err(ContextTransitionError::IrqDepthOverflow);
            }
            Ok(with_irq_depth(before.0, depth + 1))
        })?;
        let depth = transition.after.irq_depth();
        Ok(IrqEntry {
            depth,
            outermost: depth == 1,
            transition,
        })
    }

    pub(super) fn irq_exit(
        &self,
        entry: &IrqEntry,
        disposition: IrqDisposition,
    ) -> Result<ContextTransition, ContextTransitionError> {
        self.update(|before| {
            let depth = before.irq_depth();
            if depth == 0 {
                return Err(ContextTransitionError::IrqDepthUnderflow);
            }
            if depth != entry.depth {
                return Err(ContextTransitionError::IrqEntryMismatch {
                    expected_depth: entry.depth,
                    actual_depth: depth,
                });
            }

            match disposition {
                IrqDisposition::ResumeInterrupted => Ok(with_irq_depth(before.0, depth - 1)),
                IrqDisposition::ToKernel => {
                    if !entry.outermost || depth != 1 {
                        return Err(ContextTransitionError::ToKernelRequiresOutermost);
                    }
                    if before.nmi_depth() != 0 {
                        return Err(ContextTransitionError::ToKernelDuringNmi);
                    }

                    let mut next = with_irq_depth(before.0, 0);
                    if matches!(before.base(), BaseContext::Idle) {
                        if !before.idle_wait_active() || before.idle_wait_consumed() {
                            return Err(ContextTransitionError::IdleWaitProtocolCorrupt);
                        }
                        next |= IDLE_CONSUMED_BIT;
                    }
                    Ok(with_base(next, BaseContext::Kernel))
                }
            }
        })
    }

    pub(super) fn nmi_enter(&self) -> Result<NmiEntry, ContextTransitionError> {
        let transition = self.update(|before| {
            let depth = before.nmi_depth();
            if depth == MAX_DEPTH {
                return Err(ContextTransitionError::NmiDepthOverflow);
            }
            Ok(with_nmi_depth(before.0, depth + 1))
        })?;
        Ok(NmiEntry {
            depth: transition.after.nmi_depth(),
            transition,
        })
    }

    pub(super) fn nmi_exit(
        &self,
        entry: &NmiEntry,
    ) -> Result<ContextTransition, ContextTransitionError> {
        self.update(|before| {
            let depth = before.nmi_depth();
            if depth == 0 {
                return Err(ContextTransitionError::NmiDepthUnderflow);
            }
            if depth != entry.depth {
                return Err(ContextTransitionError::NmiEntryMismatch {
                    expected_depth: entry.depth,
                    actual_depth: depth,
                });
            }
            Ok(with_nmi_depth(before.0, depth - 1))
        })
    }

    fn update(
        &self,
        transition: impl Fn(ContextSnapshot) -> Result<u64, ContextTransitionError>,
    ) -> Result<ContextTransition, ContextTransitionError> {
        let mut old = self.state.load(Ordering::SeqCst);
        loop {
            let before = ContextSnapshot(old);
            let mut new = transition(before)?;
            let after_without_generation = ContextSnapshot(new);
            if before.is_watching() && after_without_generation.in_eqs() {
                new = with_generation(new, before.generation().wrapping_add(1));
            }

            match self
                .state
                .compare_exchange_weak(old, new, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => {
                    return Ok(ContextTransition {
                        before,
                        after: ContextSnapshot(new),
                    });
                }
                Err(observed) => old = observed,
            }
        }
    }
}

#[inline]
fn require_base(
    snapshot: ContextSnapshot,
    operation: ContextOperation,
    expected: BaseContext,
) -> Result<(), ContextTransitionError> {
    let actual = snapshot.base();
    if actual != expected {
        return Err(ContextTransitionError::WrongBase {
            operation,
            expected,
            actual,
        });
    }
    Ok(())
}

#[inline]
fn require_no_overlay(
    snapshot: ContextSnapshot,
    operation: ContextOperation,
) -> Result<(), ContextTransitionError> {
    if snapshot.irq_depth() != 0 || snapshot.nmi_depth() != 0 {
        return Err(ContextTransitionError::OverlayActive {
            operation,
            irq_depth: snapshot.irq_depth(),
            nmi_depth: snapshot.nmi_depth(),
        });
    }
    Ok(())
}

#[inline]
const fn with_base(word: u64, base: BaseContext) -> u64 {
    (word & !BASE_MASK) | base as u64
}

#[inline]
const fn with_irq_depth(word: u64, depth: u8) -> u64 {
    (word & !(DEPTH_MASK << IRQ_SHIFT)) | ((depth as u64) << IRQ_SHIFT)
}

#[inline]
const fn with_nmi_depth(word: u64, depth: u8) -> u64 {
    (word & !(DEPTH_MASK << NMI_SHIFT)) | ((depth as u64) << NMI_SHIFT)
}

#[inline]
const fn with_generation(word: u64, generation: u64) -> u64 {
    (word & !(GENERATION_MASK << GENERATION_SHIFT))
        | ((generation & GENERATION_MASK) << GENERATION_SHIFT)
}

pub(super) fn run_context_selftests() -> Result<(), &'static str> {
    let tracker = RcuContextTracker::new();
    if tracker.gp_report_needed() {
        return Err("RCU context tracker started with a stale GP report hint");
    }
    tracker.prepare_gp_report();
    if !tracker.gp_report_needed() {
        return Err("RCU context tracker did not publish its GP report hint");
    }
    tracker.clear_gp_report();
    let initial = tracker.snapshot();
    if initial.base() != BaseContext::Kernel || !initial.is_watching() {
        return Err("RCU context tracker did not start in Kernel watching state");
    }

    let user_enter = tracker.user_enter().map_err(|_| "user enter failed")?;
    if !user_enter.entered_eqs() || tracker.snapshot().generation() != 1 {
        return Err("Kernel-to-User did not publish an EQS generation");
    }
    let user_exit = tracker.user_exit().map_err(|_| "user exit failed")?;
    if !user_exit.exited_eqs() || tracker.snapshot().generation() != 1 {
        return Err("User-to-Kernel changed the generation or stayed in EQS");
    }

    let idle = tracker.idle_enter().map_err(|_| "idle enter failed")?;
    if !idle.transition().entered_eqs() || tracker.snapshot().generation() != 2 {
        return Err("Kernel-to-Idle did not publish an EQS generation");
    }
    let before_idle_reentry = tracker.snapshot();
    if tracker.idle_enter() != Err(ContextTransitionError::IdleWaitAlreadyActive)
        || tracker.snapshot() != before_idle_reentry
    {
        return Err("nested idle wait was not rejected atomically");
    }
    let idle_irq = tracker.irq_enter().map_err(|_| "idle IRQ enter failed")?;
    if !idle_irq.transition().exited_eqs() || !idle_irq.outermost() {
        return Err("outer idle IRQ did not restore watching");
    }
    let nested_irq = tracker.irq_enter().map_err(|_| "nested IRQ enter failed")?;
    if nested_irq.depth() != 2 || nested_irq.outermost() {
        return Err("nested IRQ entry metadata is incorrect");
    }
    let nested_exit = tracker
        .irq_exit(&nested_irq, IrqDisposition::ResumeInterrupted)
        .map_err(|_| "nested IRQ exit failed")?;
    if nested_exit.entered_eqs() || tracker.snapshot().irq_depth() != 1 {
        return Err("nested IRQ exit restored EQS too early");
    }
    let idle_irq_exit = tracker
        .irq_exit(&idle_irq, IrqDisposition::ResumeInterrupted)
        .map_err(|_| "outer idle IRQ exit failed")?;
    if !idle_irq_exit.entered_eqs() || tracker.snapshot().generation() != 3 {
        return Err("outer idle IRQ exit did not restore Idle EQS");
    }
    tracker.idle_exit().map_err(|_| "normal idle exit failed")?;

    // ToKernel consumes the EQS but leaves the idle wait active until its
    // original stack resumes. Unrelated user round-trips must preserve it.
    let _idle = tracker
        .idle_enter()
        .map_err(|_| "second idle enter failed")?;
    let consuming_irq = tracker
        .irq_enter()
        .map_err(|_| "consuming IRQ enter failed")?;
    tracker
        .irq_exit(&consuming_irq, IrqDisposition::ToKernel)
        .map_err(|_| "idle ToKernel failed")?;
    tracker
        .user_enter()
        .map_err(|_| "user enter during consumed idle wait failed")?;
    tracker
        .user_exit()
        .map_err(|_| "user exit during consumed idle wait failed")?;
    let generation_before_completion = tracker.snapshot().generation();
    let completion = tracker
        .idle_exit()
        .map_err(|_| "consumed idle completion failed")?;
    if completion.entered_eqs()
        || completion.exited_eqs()
        || tracker.snapshot().generation() != generation_before_completion
    {
        return Err("consumed idle completion changed RCU watching state");
    }
    if tracker.idle_exit() != Err(ContextTransitionError::IdleWaitMissing) {
        return Err("duplicate idle completion was not rejected");
    }

    // NMI can overlay an EQS, and an IRQ nested in that NMI cannot select
    // ToKernel because only the outer NMI may restore the interrupted base.
    tracker.user_enter().map_err(|_| "NMI user setup failed")?;
    let nmi = tracker.nmi_enter().map_err(|_| "NMI enter failed")?;
    if !nmi.transition().exited_eqs() || nmi.depth() != 1 {
        return Err("NMI over User did not restore watching");
    }
    let irq_over_nmi = tracker
        .irq_enter()
        .map_err(|_| "IRQ-over-NMI enter failed")?;
    let before_rejected_exit = tracker.snapshot();
    if tracker.irq_exit(&irq_over_nmi, IrqDisposition::ToKernel)
        != Err(ContextTransitionError::ToKernelDuringNmi)
        || tracker.snapshot() != before_rejected_exit
    {
        return Err("IRQ-over-NMI ToKernel was not rejected atomically");
    }
    tracker
        .irq_exit(&irq_over_nmi, IrqDisposition::ResumeInterrupted)
        .map_err(|_| "IRQ-over-NMI recovery exit failed")?;
    let nmi_exit = tracker.nmi_exit(&nmi).map_err(|_| "NMI exit failed")?;
    if !nmi_exit.entered_eqs() {
        return Err("outer NMI exit did not restore User EQS");
    }
    tracker.user_exit().map_err(|_| "NMI user cleanup failed")?;

    // Exercise nested NMI and the stronger Idle -> NMI -> IRQ rejection path.
    tracker.idle_enter().map_err(|_| "idle/NMI setup failed")?;
    let outer_nmi = tracker
        .nmi_enter()
        .map_err(|_| "outer nested-NMI enter failed")?;
    let inner_nmi = tracker
        .nmi_enter()
        .map_err(|_| "inner nested-NMI enter failed")?;
    if inner_nmi.depth() != 2 {
        return Err("nested NMI depth is incorrect");
    }
    tracker
        .nmi_exit(&inner_nmi)
        .map_err(|_| "inner nested-NMI exit failed")?;
    let idle_irq_over_nmi = tracker
        .irq_enter()
        .map_err(|_| "idle IRQ-over-NMI enter failed")?;
    let idle_overlay_snapshot = tracker.snapshot();
    if tracker.irq_exit(&idle_irq_over_nmi, IrqDisposition::ToKernel)
        != Err(ContextTransitionError::ToKernelDuringNmi)
        || tracker.snapshot() != idle_overlay_snapshot
    {
        return Err("Idle NMI/IRQ overlay ToKernel was not rejected atomically");
    }
    tracker
        .irq_exit(&idle_irq_over_nmi, IrqDisposition::ResumeInterrupted)
        .map_err(|_| "idle IRQ-over-NMI recovery failed")?;
    let idle_nmi_exit = tracker
        .nmi_exit(&outer_nmi)
        .map_err(|_| "outer nested-NMI exit failed")?;
    if !idle_nmi_exit.entered_eqs() {
        return Err("NMI exit did not restore Idle EQS");
    }
    tracker.idle_exit().map_err(|_| "idle/NMI cleanup failed")?;

    let before_illegal = tracker.snapshot();
    if tracker.user_exit().is_ok() || tracker.snapshot() != before_illegal {
        return Err("illegal user exit changed context state");
    }
    if tracker.nmi_exit(&NmiEntry {
        depth: 1,
        transition: ContextTransition {
            before: before_illegal,
            after: before_illegal,
        },
    }) != Err(ContextTransitionError::NmiDepthUnderflow)
        || tracker.snapshot() != before_illegal
    {
        return Err("NMI underflow did not preserve context state");
    }

    let irq_underflow_entry = IrqEntry {
        depth: 1,
        outermost: true,
        transition: ContextTransition {
            before: before_illegal,
            after: before_illegal,
        },
    };
    if tracker.irq_exit(&irq_underflow_entry, IrqDisposition::ResumeInterrupted)
        != Err(ContextTransitionError::IrqDepthUnderflow)
        || tracker.snapshot() != before_illegal
    {
        return Err("IRQ underflow did not preserve context state");
    }

    let overflow_irq = RcuContextTracker {
        state: AtomicU64::new(with_irq_depth(BaseContext::Kernel as u64, MAX_DEPTH)),
        gp_report_needed: AtomicBool::new(false),
    };
    if overflow_irq.irq_enter() != Err(ContextTransitionError::IrqDepthOverflow) {
        return Err("IRQ depth overflow was not rejected");
    }
    let overflow_nmi = RcuContextTracker {
        state: AtomicU64::new(with_nmi_depth(BaseContext::Kernel as u64, MAX_DEPTH)),
        gp_report_needed: AtomicBool::new(false),
    };
    if overflow_nmi.nmi_enter() != Err(ContextTransitionError::NmiDepthOverflow) {
        return Err("NMI depth overflow was not rejected");
    }

    let mismatch = RcuContextTracker::new();
    let outer_irq = mismatch
        .irq_enter()
        .map_err(|_| "IRQ mismatch setup failed")?;
    let inner_irq = mismatch
        .irq_enter()
        .map_err(|_| "nested IRQ mismatch setup failed")?;
    let before_mismatch = mismatch.snapshot();
    if mismatch.irq_exit(&outer_irq, IrqDisposition::ResumeInterrupted)
        != Err(ContextTransitionError::IrqEntryMismatch {
            expected_depth: 1,
            actual_depth: 2,
        })
        || mismatch.snapshot() != before_mismatch
    {
        return Err("out-of-order IRQ token did not preserve state");
    }
    if mismatch.irq_exit(&inner_irq, IrqDisposition::ToKernel)
        != Err(ContextTransitionError::ToKernelRequiresOutermost)
        || mismatch.snapshot() != before_mismatch
    {
        return Err("nested IRQ ToKernel was not rejected atomically");
    }
    mismatch
        .irq_exit(&inner_irq, IrqDisposition::ResumeInterrupted)
        .map_err(|_| "nested IRQ mismatch cleanup failed")?;
    mismatch
        .irq_exit(&outer_irq, IrqDisposition::ResumeInterrupted)
        .map_err(|_| "outer IRQ mismatch cleanup failed")?;

    Ok(())
}
