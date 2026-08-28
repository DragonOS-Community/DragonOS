use super::{PtraceEvent, PtraceOptions};
use crate::{arch::ipc::signal::Signal, ipc::signal_types::SigInfo, process::ProcessControlBlock};
use alloc::sync::Arc;
use system_error::SystemError;

// ptrace exit_code / si_code encoding
pub(super) const EXITCODE_SIG_MASK: usize = 0x7f;
/// Shift of the event encoding within exit_code.
pub(super) const EXITCODE_EVENT_SHIFT: u32 = 8;
/// sysgood flag bit for syscall-stop (requires PTRACE_O_TRACESYSGOOD).
pub(super) const PTRACE_SYSGOOD_BIT: usize = 0x80;

/// A complete snapshot of one ptrace-stop. The event message and mutable siginfo must be
/// published together with the generation; do not assemble state from different stops using independent fields.
#[derive(Debug)]
struct PtraceStopRecord {
    generation: u64,
    exit_code: usize,
    mutable_siginfo: Option<SigInfo>,
    event_message: usize,
    /// Generation of the shared group-stop transaction completed by this
    /// report.  Detach uses it to distinguish a live group-stop from one
    /// cancelled by SIGCONT after the report was published.
    group_stop_generation: Option<u64>,
    report_pending: bool,
}

/// The one scheduler stop currently owned by ptrace.  LISTEN keeps the same
/// stop generation alive, but deliberately hides it from ptrace requests and
/// tracer wait.  Pending re-traps and old waiter completions are orthogonal to
/// this phase and therefore remain separate fields in `PtraceState`.
#[derive(Debug)]
enum ActiveStop {
    Traced(PtraceStopRecord),
    Listening(PtraceStopRecord),
}

impl ActiveStop {
    fn record(&self) -> &PtraceStopRecord {
        match self {
            Self::Traced(stop) | Self::Listening(stop) => stop,
        }
    }

    fn into_record(self) -> PtraceStopRecord {
        match self {
            Self::Traced(stop) | Self::Listening(stop) => stop,
        }
    }
}

/// The tracer has consumed one generation of stop, but the tracee has not yet returned from schedule().
/// The generation ensures an old waiter can only take away its own resume result.
#[derive(Debug)]
struct PtraceResumeRecord {
    generation: u64,
    injected_signal: Signal,
    mutable_siginfo: Option<SigInfo>,
}

/// A sticky trap request is owned by the tracing session that published it.
/// The tracee may take this record just before detach; the generation check in
/// ptrace_stop() prevents that in-flight record from entering a later session.
#[derive(Debug, Clone, Copy)]
pub(super) enum PendingStopKind {
    Notify,
    Group { generation: u64, counted: bool },
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PendingStopRecord {
    pub(super) signal: Signal,
    pub(super) session_generation: u64,
    pub(super) kind: PendingStopKind,
}

/// Identity of the ptrace-stop frozen by one in-flight ptrace request.
///
/// Both generations are needed: `stop_generation` prevents a request from
/// releasing a later stop in the same tracing session, while
/// `session_generation` prevents it from affecting a detach/reattach session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PtraceFreezeToken {
    pub(super) session_generation: u64,
    pub(super) stop_generation: u64,
}

#[derive(Debug)]
struct PtraceFreezeOwner {
    token: PtraceFreezeToken,
    deferred_fatal_wake: bool,
}

/// Scheduler side effects produced when a transition releases or revokes a
/// request freeze.  The state transition happens under `ptrace_state`; the
/// wakeup must happen after all ptrace/relation/signal locks have been dropped.
#[must_use = "deferred fatal wakeups must be replayed after dropping ptrace locks"]
#[derive(Debug, Default)]
pub(super) struct PtraceReleaseOutcome {
    pub(super) fatal_wake: bool,
}

impl PtraceReleaseOutcome {
    pub(super) fn apply(self, tracee: &Arc<ProcessControlBlock>) {
        if self.fatal_wake {
            crate::ipc::signal::replay_deferred_fatal_wake(tracee.clone());
        }
    }
}

/// Stop-side facts committed while tearing down one tracing session.  The
/// caller uses `had_active_stop` after dropping ptrace locks to decide whether
/// the scheduler stop needs to be preserved or woken.
#[must_use = "session reset outcomes carry deferred scheduler side effects"]
#[derive(Debug)]
pub(super) struct PtraceSessionResetOutcome {
    pub(super) had_active_stop: bool,
    pub(super) active_group_stop: Option<u64>,
    pub(super) pending_group_stop: Option<(u64, Signal, bool)>,
    pub(super) freeze_release: PtraceReleaseOutcome,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingDebugSignal {
    pub bits: u64,
    pub icebp: bool,
    pub addr: usize,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PendingDebugRecord {
    pub(super) owned_bits: u64,
    pub(super) unowned_bits: u64,
    pub(super) icebp: bool,
    pub(super) addr: usize,
    pub(super) owner_generation: u64,
}

// PtraceState -- tracking state machine (corresponding to the ptrace/jobctl-related fields of Linux task_struct)
/// State information for a process being traced by ptrace.
#[derive(Debug)]
pub struct PtraceState {
    active_stop: Option<ActiveStop>,
    completed_resume: Option<PtraceResumeRecord>,
    next_stop_generation: u64,
    /// ptrace options (PTRACE_O_*).
    pub options: PtraceOptions,
    /// Generation-bound owner of the current request-level freeze.
    freeze_owner: Option<PtraceFreezeOwner>,
    /// Sticky PTRACE_EVENT_STOP payload. It can coexist with either active-stop
    /// phase and guarantees another stop after the current one is left.
    pending_stop: Option<PendingStopRecord>,
    /// TIF_FORCED_TF: true indicates the current TF was forcibly set by the debugger for single-step.
    pub forced_trap_flag: bool,
    /// Whether the user TrapFrame of the current ptrace-stop is on the syscall stack.
    /// The rsp saved by the scheduler context cannot be used to guess the TrapFrame location.
    pub stop_frame_on_syscall_stack: bool,
    /// EXITKILL verdict bit (doom bit): set within the same critical section that clears the
    /// relation when the old tracer exits and this session had PTRACE_O_EXITKILL set,
    /// meaning "this tracee has been sentenced to death by the old session; SIGKILL is pending".
    exitkill_pending: bool,
    /// ptrace-side storage for the debug registers (DR0-DR7).
    pub debug_regs: [u64; 8],
    /// Fixed-size #DB handoff from exception context to return-to-user.
    pub(super) pending_debug: Option<PendingDebugRecord>,
}

impl Default for PtraceState {
    fn default() -> Self {
        Self {
            active_stop: None,
            completed_resume: None,
            next_stop_generation: 0,
            options: PtraceOptions::empty(),
            freeze_owner: None,
            pending_stop: None,
            forced_trap_flag: false,
            stop_frame_on_syscall_stack: false,
            exitkill_pending: false,
            debug_regs: [0; 8],
            pending_debug: None,
        }
    }
}

impl PtraceState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish an EXITKILL verdict while relation teardown owns the task.
    /// Only the relation module may invoke this transition.
    pub(super) fn publish_exitkill_verdict(&mut self) {
        self.exitkill_pending = true;
    }

    /// Consume an EXITKILL verdict exactly once while holding the relation lock.
    /// The verdict intentionally survives ordinary stop/session reset.
    pub(super) fn take_exitkill_verdict(&mut self) -> bool {
        core::mem::take(&mut self.exitkill_pending)
    }

    pub fn consume_stop_report(&mut self, consume: bool) -> Option<i32> {
        let Some(ActiveStop::Traced(stop)) = self.active_stop.as_mut() else {
            return None;
        };
        if !stop.report_pending {
            return None;
        }
        let code = stop.exit_code as i32;
        if consume {
            stop.report_pending = false;
        }
        Some(code)
    }

    pub(super) fn publish_stop(
        &mut self,
        exit_code: usize,
        mutable_siginfo: Option<SigInfo>,
        event_message: usize,
        group_stop_generation: Option<u64>,
    ) -> u64 {
        self.next_stop_generation = self.next_stop_generation.wrapping_add(1);
        if self.next_stop_generation == 0 {
            self.next_stop_generation = 1;
        }
        let generation = self.next_stop_generation;
        self.active_stop = Some(ActiveStop::Traced(PtraceStopRecord {
            generation,
            exit_code,
            mutable_siginfo,
            event_message,
            group_stop_generation,
            report_pending: true,
        }));
        generation
    }

    fn active_stop_generation(&self) -> Option<u64> {
        self.active_stop
            .as_ref()
            .map(|active| active.record().generation)
    }

    pub(crate) fn is_traced_stop(&self) -> bool {
        matches!(self.active_stop, Some(ActiveStop::Traced(_)))
    }

    pub(super) fn has_active_stop(&self) -> bool {
        self.active_stop.is_some()
    }

    pub(super) fn active_group_stop_generation(&self) -> Option<u64> {
        self.active_stop
            .as_ref()
            .and_then(|active| active.record().group_stop_generation)
    }

    pub(super) fn pending_stop_needs_wake(&self) -> bool {
        !self.is_traced_stop()
    }

    pub(super) fn queue_pending_stop(
        &mut self,
        signal: Signal,
        session_generation: u64,
        current_group_stop: Option<u64>,
    ) {
        // An interrupt/notify must not consume an in-flight group-stop ticket.
        // A stale ticket (cancelled by SIGCONT) may be replaced normally.
        if matches!(
            self.pending_stop,
            Some(PendingStopRecord {
                kind: PendingStopKind::Group { generation, .. },
                ..
            }) if Some(generation) == current_group_stop
        ) {
            return;
        }
        self.pending_stop = Some(PendingStopRecord {
            signal,
            session_generation,
            kind: PendingStopKind::Notify,
        });
    }

    fn queue_pending_group_stop_with_count(
        &mut self,
        signal: Signal,
        session_generation: u64,
        generation: u64,
        counted: bool,
    ) -> bool {
        if matches!(
            self.pending_stop,
            Some(PendingStopRecord {
                kind: PendingStopKind::Group {
                    generation: queued,
                    ..
                },
                ..
            }) if queued == generation
        ) {
            return false;
        }
        self.pending_stop = Some(PendingStopRecord {
            signal,
            session_generation,
            kind: PendingStopKind::Group {
                generation,
                counted,
            },
        });
        true
    }

    pub(super) fn queue_pending_group_stop(
        &mut self,
        signal: Signal,
        session_generation: u64,
        generation: u64,
    ) -> bool {
        self.queue_pending_group_stop_with_count(signal, session_generation, generation, true)
    }

    /// A thread cloned into an already completed group-stop still owes a
    /// reportable stop, but must not reopen or increment that transaction.
    pub(super) fn queue_completed_group_stop(
        &mut self,
        signal: Signal,
        session_generation: u64,
        generation: u64,
    ) -> bool {
        self.queue_pending_group_stop_with_count(signal, session_generation, generation, false)
    }

    pub(super) fn take_pending_stop(&mut self) -> Option<PendingStopRecord> {
        self.pending_stop.take()
    }

    pub(super) fn clear_pending_stop(&mut self) {
        self.pending_stop = None;
    }

    pub(super) fn install_freeze_owner(
        &mut self,
        session_generation: u64,
    ) -> Result<PtraceFreezeToken, SystemError> {
        if self.freeze_owner.is_some() || !self.is_traced_stop() {
            return Err(SystemError::ESRCH);
        }
        let stop_generation = self.active_stop_generation().ok_or(SystemError::ESRCH)?;
        let token = PtraceFreezeToken {
            session_generation,
            stop_generation,
        };
        self.freeze_owner = Some(PtraceFreezeOwner {
            token,
            deferred_fatal_wake: false,
        });
        Ok(token)
    }

    pub(super) fn freeze_owner_matches(&self, token: PtraceFreezeToken) -> bool {
        self.freeze_owner
            .as_ref()
            .map(|owner| owner.token == token)
            .unwrap_or(false)
            && self.active_stop_generation() == Some(token.stop_generation)
            && self.is_traced_stop()
    }

    /// Return true when a request freeze owns the fatal wake and has deferred
    /// it. The caller must then leave the stopped task asleep until the owner
    /// is released or revoked.
    fn defer_fatal_wake_to_freeze_owner(&mut self) -> bool {
        let Some(owner) = self.freeze_owner.as_mut() else {
            return false;
        };
        owner.deferred_fatal_wake = true;
        true
    }

    fn finish_freeze_release(&mut self, owner: PtraceFreezeOwner) -> PtraceReleaseOutcome {
        if owner.deferred_fatal_wake {
            // Match Linux ptrace_unfreeze_traced(): a pending fatal signal
            // ends the active stop before waking the tracee. Preserve the
            // generation-bound waiter result instead of leaving a reportable
            // stop record behind after the scheduler wake.
            self.abort_active_stop();
        }
        PtraceReleaseOutcome {
            fatal_wake: owner.deferred_fatal_wake,
        }
    }

    pub(super) fn release_freeze_owner(
        &mut self,
        token: PtraceFreezeToken,
    ) -> PtraceReleaseOutcome {
        let Some(owner) = self.freeze_owner.take() else {
            return PtraceReleaseOutcome::default();
        };
        if owner.token != token {
            self.freeze_owner = Some(owner);
            return PtraceReleaseOutcome::default();
        }
        self.finish_freeze_release(owner)
    }

    pub(super) fn revoke_freeze_owner(&mut self) -> PtraceReleaseOutcome {
        let Some(owner) = self.freeze_owner.take() else {
            return PtraceReleaseOutcome::default();
        };
        self.finish_freeze_release(owner)
    }

    /// Gate a fatal wake behind an in-flight request, or end the current stop
    /// immediately when no request owns it. Returns true when the actual wake
    /// was deferred to the freeze owner.
    pub(crate) fn prepare_fatal_wake(&mut self) -> bool {
        if self.defer_fatal_wake_to_freeze_owner() {
            return true;
        }
        self.abort_active_stop();
        false
    }

    fn abort_active_stop(&mut self) {
        let Some(active) = self.active_stop.take() else {
            return;
        };
        let stop = active.into_record();
        // Only one tracee waiter can be returning from schedule(). If a prior
        // generation is already awaiting collection, keep that older result;
        // finish_waiter() must never mistake the newer generation for it.
        if self.completed_resume.is_none() {
            self.completed_resume = Some(PtraceResumeRecord {
                generation: stop.generation,
                injected_signal: Signal::INVALID,
                mutable_siginfo: stop.mutable_siginfo,
            });
        }
    }

    pub(super) fn prepare_resume(&mut self, injected_signal: Signal) -> Result<(), SystemError> {
        let active = self.active_stop.take().ok_or(SystemError::ESRCH)?;
        let stop = match active {
            ActiveStop::Traced(stop) => stop,
            listening @ ActiveStop::Listening(_) => {
                self.active_stop = Some(listening);
                return Err(SystemError::ESRCH);
            }
        };
        // The same tracee must not consume a new stop before returning from the old schedule();
        // refusing to overwrite guarantees an old waiter can never mistake the new-generation result.
        if self.completed_resume.is_some() {
            self.active_stop = Some(ActiveStop::Traced(stop));
            return Err(SystemError::ESRCH);
        }
        self.completed_resume = Some(PtraceResumeRecord {
            generation: stop.generation,
            injected_signal,
            mutable_siginfo: stop.mutable_siginfo,
        });
        Ok(())
    }

    pub(super) fn finish_waiter(&mut self, generation: u64) -> (Option<SigInfo>, Signal) {
        if self
            .completed_resume
            .as_ref()
            .map(|resume| resume.generation == generation)
            .unwrap_or(false)
        {
            let resume = self.completed_resume.take().unwrap();
            return (resume.mutable_siginfo, resume.injected_signal);
        }
        if self
            .active_stop
            .as_ref()
            .map(|active| active.record().generation == generation)
            .unwrap_or(false)
        {
            let stop = self.active_stop.take().unwrap().into_record();
            return (stop.mutable_siginfo, Signal::INVALID);
        }
        // Never clean up a new-generation stop once it has been published; an old waiter returns with no injected signal.
        (None, Signal::INVALID)
    }

    pub(super) fn stop_siginfo(&self) -> Option<SigInfo> {
        match self.active_stop.as_ref() {
            Some(ActiveStop::Traced(stop)) => stop.mutable_siginfo,
            _ => None,
        }
    }

    pub(super) fn stop_siginfo_mut(&mut self) -> Option<&mut SigInfo> {
        match self.active_stop.as_mut() {
            Some(ActiveStop::Traced(stop)) => stop.mutable_siginfo.as_mut(),
            _ => None,
        }
    }

    pub(super) fn stop_event_message(&self) -> usize {
        match self.active_stop.as_ref() {
            Some(ActiveStop::Traced(stop)) => stop.event_message,
            _ => 0,
        }
    }

    pub(super) fn enter_listening(
        &mut self,
        token: PtraceFreezeToken,
    ) -> Result<(Signal, bool, PtraceReleaseOutcome), SystemError> {
        if !self.freeze_owner_matches(token) {
            return Err(SystemError::ESRCH);
        }
        let active = self.active_stop.take().ok_or(SystemError::ESRCH)?;
        let mut stop = match active {
            ActiveStop::Traced(stop) => stop,
            listening @ ActiveStop::Listening(_) => {
                self.active_stop = Some(listening);
                return Err(SystemError::ESRCH);
            }
        };
        let is_event_stop = stop
            .mutable_siginfo
            .map(|info| (info.sig_code().as_i32() >> 8) == PtraceEvent::Stop as i32)
            .unwrap_or(false);
        if !is_event_stop {
            self.active_stop = Some(ActiveStop::Traced(stop));
            return Err(SystemError::EIO);
        }

        let stop_signal = Signal::from((stop.exit_code & EXITCODE_SIG_MASK) as i32);
        let retrap = self.pending_stop.is_some();
        stop.report_pending = false;
        self.active_stop = Some(ActiveStop::Listening(stop));
        let release = self.release_freeze_owner(token);
        Ok((stop_signal, retrap, release))
    }

    /// The only stop/reset entry point when tearing down a ptrace session.
    pub(super) fn reset_session_stop(&mut self) -> PtraceSessionResetOutcome {
        let had_active_stop = self.has_active_stop();
        let active_group_stop = self.active_group_stop_generation();
        let pending_group_stop = self.pending_stop.and_then(|pending| match pending.kind {
            PendingStopKind::Notify => None,
            PendingStopKind::Group {
                generation,
                counted,
            } => Some((generation, pending.signal, counted)),
        });
        let release = self.revoke_freeze_owner();
        // A waiter blocked in ptrace_stop() needs a generation-bound
        // result to return safely.
        self.abort_active_stop();
        self.clear_pending_stop();
        PtraceSessionResetOutcome {
            had_active_stop,
            active_group_stop,
            pending_group_stop,
            freeze_release: release,
        }
    }
}

impl ProcessControlBlock {
    /// Whether wait/signal code currently observes a reportable ptrace-stop.
    pub(crate) fn is_in_ptrace_stop(&self) -> bool {
        self.ptrace.state.lock_irqsave().is_traced_stop()
    }

    /// Serialize a fatal wake with any request that currently freezes the stop.
    pub(crate) fn prepare_ptrace_fatal_wake(&self) -> bool {
        self.ptrace.state.lock_irqsave().prepare_fatal_wake()
    }
}
