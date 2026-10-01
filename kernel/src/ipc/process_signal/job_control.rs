//! Group stop, continue, exit and parent notification transactions.
use super::*;

impl ProcessSignalState {
    /// Run a narrow group-stop transaction while the shared signal state is
    /// write-locked. Ptrace callers acquire `PTRACE_RELATION_LOCK` first.
    pub(crate) fn with_group_stop_state<R>(
        &self,
        f: impl FnOnce(&mut InnerProcessSignalState) -> R,
    ) -> R {
        let mut guard = self.inner_mut();
        f(&mut guard)
    }

    /// Apply a job-control stop and publish its wait event as one state
    /// transition. The callback runs while the shared process signal state is locked, so a
    /// concurrent SIGCONT cannot wake the group between the scheduler-state
    /// update and the persistent stop-state publication.
    ///
    /// Returns whether this completed a fresh group stop which should be
    /// reported to the parent. Repeated stop signals keep the existing event.
    pub fn transition_group_stop<F>(&self, sig: Signal, stop_group: F) -> bool
    where
        F: FnOnce() -> bool,
    {
        let mut g = self.inner_mut();
        if g.flags.contains(SignalFlags::GROUP_EXIT) || !stop_group() {
            return false;
        }
        if g.flags.contains(SignalFlags::STOP_STOPPED) {
            return false;
        }

        g.stop_signal = sig;
        g.flags.remove(SignalFlags::STOP_MASK);
        g.flags
            .insert(SignalFlags::STOP_STOPPED | SignalFlags::CLD_STOPPED);
        true
    }

    /// Continue a job-control-stopped group as one transition. The callback is
    /// run whenever group exit has not started because SIGCONT resumes stopped
    /// tasks even when no parent notification is pending. A continued event is
    /// generated only for a completed group stop, matching Linux's
    /// SIGNAL_STOP_STOPPED test.
    pub fn transition_group_continue<F>(&self, continue_group: F) -> bool
    where
        F: FnOnce(),
    {
        let mut g = self.inner_mut();
        if g.flags.contains(SignalFlags::GROUP_EXIT) {
            return false;
        }
        let was_stopped = g.flags.contains(SignalFlags::STOP_STOPPED);

        if was_stopped || g.group_stop_pending_ptraced != 0 {
            g.advance_group_stop_generation();
            g.group_stop_pending_ptraced = 0;
        }

        continue_group();
        if was_stopped {
            g.flags.remove(SignalFlags::STOP_MASK);
            g.flags
                .insert(SignalFlags::STOP_CONTINUED | SignalFlags::CLD_CONTINUED);
        }
        was_stopped
    }

    /// Observe the persistent natural-child group-stop event. The stop state,
    /// stop signal, and optional consumption are protected by one lock, as are
    /// Linux's SIGNAL_STOP_STOPPED and group_exit_code checks.
    pub fn group_stop_event(&self, consume: bool) -> Option<Signal> {
        let mut g = self.inner_mut();
        if !g.flags.contains(SignalFlags::STOP_STOPPED)
            || !g.flags.contains(SignalFlags::CLD_STOPPED)
        {
            return None;
        }

        let sig = g.stop_signal;
        if consume {
            g.flags.remove(SignalFlags::CLD_STOPPED);
        }
        Some(sig)
    }

    /// Observe a ptrace stop without imposing the natural-child
    /// STOP_STOPPED requirement. The scheduler-state recheck, event code, and
    /// optional consumption stay in the same process signal state critical section.
    pub fn ptrace_stop_event<F>(&self, consume: bool, is_stopped: F) -> Option<Signal>
    where
        F: FnOnce() -> bool,
    {
        let mut g = self.inner_mut();
        if !is_stopped() || !g.flags.contains(SignalFlags::CLD_STOPPED) {
            return None;
        }

        let sig = g.stop_signal;
        if consume {
            g.flags.remove(SignalFlags::CLD_STOPPED);
        }
        Some(sig)
    }

    pub fn stop_signal(&self) -> Signal {
        self.inner().stop_signal
    }

    pub fn set_stop_signal(&self, sig: Signal) {
        let mut g = self.inner_mut();
        g.stop_signal = sig;
    }

    /// Claim the leader's natural-parent notification responsibility while
    /// holding the same lock used by group-exec/reap arbitration. `eligible`
    /// may acquire the leader thread-info lock (`ProcessSignalState -> thread-info`).
    pub fn try_claim_natural_parent_notify<F>(
        &self,
        candidate: &Arc<ProcessControlBlock>,
        eligible: F,
    ) -> Option<NaturalParentNotifyToken>
    where
        F: FnOnce() -> bool,
    {
        self.try_claim_natural_parent_notify_with(candidate, || ((), eligible()))
            .1
    }

    /// Variant for the last-sibling unhash path. `transition` always runs
    /// while the process signal state lock is held, so it can remove the sibling under the
    /// nested leader thread-info lock and return whether that removal made a
    /// Zombie leader eligible for natural-parent notification.
    pub fn try_claim_natural_parent_notify_with<F, R>(
        &self,
        candidate: &Arc<ProcessControlBlock>,
        transition: F,
    ) -> (R, Option<NaturalParentNotifyToken>)
    where
        F: FnOnce() -> (R, bool),
    {
        let g = self.inner_mut();
        let (result, eligible) = transition();
        if !candidate.is_thread_group_leader()
            || (g.flags.contains(SignalFlags::GROUP_EXEC)
                && Self::weak_matches(&g.group_exec_old_leader, candidate))
            || !eligible
        {
            return (result, None);
        }
        let owner = Arc::downgrade(candidate);
        let token = candidate
            .try_claim_natural_parent_notify()
            .then_some(NaturalParentNotifyToken { owner });
        (result, token)
    }

    pub fn complete_natural_parent_notify(&self, token: NaturalParentNotifyToken) -> bool {
        token
            .owner
            .upgrade()
            .map(|owner| owner.complete_natural_parent_notify())
            .unwrap_or(false)
    }

    /// Stable report/reap decision for natural-parent wait and autoreap.
    pub fn try_reap_natural_child(
        &self,
        candidate: &Arc<ProcessControlBlock>,
        consume: bool,
    ) -> ReapTransition {
        self.try_reap_natural_child_inner(candidate, consume, None)
    }

    /// Read-only fast probe used by wait scans before attempting a consuming
    /// transition. The consuming helper rechecks these barriers under the
    /// write lock, so a concurrent transaction cannot slip through a TOCTOU
    /// window.
    pub fn natural_reap_blocked(&self, candidate: &Arc<ProcessControlBlock>) -> bool {
        let g = self.inner();
        (g.flags.contains(SignalFlags::GROUP_EXEC)
            && Self::weak_matches(&g.group_exec_old_leader, candidate))
            || candidate.natural_parent_notify_phase() == NaturalParentNotifyPhase::Pending
    }

    /// Stable ptrace report/reap decision. Group-exec arbitration and the
    /// optional Zombie -> Dead transition share one ProcessSignalState critical section.
    pub fn try_reap_ptraced_child(
        &self,
        candidate: &Arc<ProcessControlBlock>,
        consume: bool,
    ) -> ReapTransition {
        let g = self.inner_mut();
        if g.flags.contains(SignalFlags::GROUP_EXEC)
            && Self::weak_matches(&g.group_exec_old_leader, candidate)
        {
            return ReapTransition::Blocked;
        }
        if !candidate.is_zombie() {
            return ReapTransition::NotZombie;
        }
        if !consume {
            return ReapTransition::Reportable;
        }
        if candidate.try_mark_dead_from_zombie() {
            ReapTransition::Reaped
        } else {
            ReapTransition::NotZombie
        }
    }

    /// Claim a ptraced zombie for a consuming wait without publishing Dead
    /// yet.  This mirrors Linux's EXIT_ZOMBIE -> EXIT_TRACE cmpxchg: the owner
    /// may safely detach before deciding whether the natural parent needs a
    /// second zombie report.
    pub fn try_claim_ptraced_child(&self, candidate: &Arc<ProcessControlBlock>) -> ReapTransition {
        let g = self.inner_mut();
        if g.flags.contains(SignalFlags::GROUP_EXEC)
            && Self::weak_matches(&g.group_exec_old_leader, candidate)
        {
            return ReapTransition::Blocked;
        }
        if candidate.try_claim_trace_zombie() {
            ReapTransition::TraceClaimed
        } else {
            ReapTransition::NotZombie
        }
    }

    /// Autoreap used by the unique natural-parent notification owner. The
    /// token bypasses only its own Pending barrier, never a group-exec barrier.
    pub fn try_reap_natural_child_as_notify_owner(
        &self,
        candidate: &Arc<ProcessControlBlock>,
        token: &NaturalParentNotifyToken,
    ) -> ReapTransition {
        self.try_reap_natural_child_inner(candidate, true, Some(token))
    }

    pub fn reap_blocked_by_group_exec(&self, candidate: &Arc<ProcessControlBlock>) -> bool {
        let g = self.inner();
        g.flags.contains(SignalFlags::GROUP_EXEC)
            && Self::weak_matches(&g.group_exec_old_leader, candidate)
    }

    /// 若当前线程组已经处于 group-exit 状态，则返回统一的退出码；否则返回 None
    pub fn group_exit_code_if_set(&self) -> Option<usize> {
        let g = self.inner();
        if g.flags.contains(SignalFlags::GROUP_EXIT) {
            Some(g.group_exit_code)
        } else {
            None
        }
    }

    /// 启动线程组退出：
    /// - 若此前尚未标记 GROUP_EXIT，则设置标志与退出码，并返回本次传入的退出码
    /// - 若已经有线程设置了 GROUP_EXIT，则直接返回已存在的 group_exit_code
    pub fn start_group_exit(&self, exit_code: usize) -> usize {
        let mut g = self.inner_mut();
        if g.flags.contains(SignalFlags::GROUP_EXIT) {
            g.group_exit_code
        } else {
            // Linux do_group_exit() replaces signal->flags with
            // SIGNAL_GROUP_EXIT, discarding all job-control wait state.
            g.cancel_group_stop();
            g.flags.remove(SignalFlags::STOP_MASK);
            g.flags.insert(SignalFlags::GROUP_EXIT);
            g.group_exit_code = exit_code;
            exit_code
        }
    }

    /// Initiates thread group exit triggered by a fatal signal.
    ///
    /// In Linux, the fatal group-exit branch in `complete_signal()` overwrites
    /// stop/job-control state. DragonOS currently lacks a full jobctl structure,
    /// but the stopped/continued state visible to wait is stored in
    /// `SignalFlags::STOP_MASK`; we clear it here before setting GROUP_EXIT to
    /// prevent a soon-to-be-killed stopped thread group from exposing stale
    /// stop/continue events.
    ///
    /// Returns true only for the caller that actually transitions the group
    /// into GROUP_EXIT.
    pub fn start_group_exit_for_fatal_signal(&self, exit_code: usize) -> bool {
        let mut g = self.inner_mut();
        if g.flags.contains(SignalFlags::GROUP_EXIT) {
            false
        } else {
            g.cancel_group_stop();
            g.flags.remove(SignalFlags::STOP_MASK);
            g.flags.insert(SignalFlags::GROUP_EXIT);
            g.group_exit_code = exit_code;
            true
        }
    }
}

impl InnerProcessSignalState {
    fn advance_group_stop_generation(&mut self) -> u64 {
        self.group_stop_generation = self.group_stop_generation.wrapping_add(1);
        if self.group_stop_generation == 0 {
            self.group_stop_generation = 1;
        }
        self.group_stop_generation
    }

    /// Begin a fresh group-stop with the current ptraced task as its first
    /// asynchronous participant. Repeated stop signals do not restart a
    /// completed or already in-flight transaction.
    pub(crate) fn begin_ptrace_group_stop(&mut self, signal: Signal) -> Option<u64> {
        if self.flags.contains(SignalFlags::GROUP_EXIT)
            || self.flags.contains(SignalFlags::STOP_STOPPED)
            || self.group_stop_pending_ptraced != 0
        {
            return None;
        }
        let generation = self.advance_group_stop_generation();
        self.stop_signal = signal;
        self.flags.remove(SignalFlags::STOP_MASK);
        self.group_stop_pending_ptraced = 1;
        Some(generation)
    }

    pub(crate) fn add_ptrace_group_stop_participant(&mut self, generation: u64) -> bool {
        if self.group_stop_generation != generation || self.group_stop_pending_ptraced == 0 {
            return false;
        }
        self.group_stop_pending_ptraced += 1;
        true
    }

    pub(crate) fn ptrace_group_stop_in_progress(&self, generation: u64) -> bool {
        self.group_stop_generation == generation && self.group_stop_pending_ptraced != 0
    }

    pub(crate) fn ptrace_group_stop_is_current(&self, generation: u64) -> bool {
        self.group_stop_generation == generation
            && (self.group_stop_pending_ptraced != 0
                || self.flags.contains(SignalFlags::STOP_STOPPED))
    }

    /// Complete exactly one generation-bound ptrace participant. The last
    /// participant publishes the existing wait-visible STOP flags.
    pub(crate) fn complete_ptrace_group_stop(&mut self, generation: u64) -> bool {
        if !self.ptrace_group_stop_in_progress(generation) {
            return false;
        }
        self.group_stop_pending_ptraced -= 1;
        if self.group_stop_pending_ptraced == 0 {
            self.flags
                .insert(SignalFlags::STOP_STOPPED | SignalFlags::CLD_STOPPED);
            return true;
        }
        false
    }

    pub(crate) fn current_incomplete_group_stop(&self) -> Option<u64> {
        (self.group_stop_pending_ptraced != 0).then_some(self.group_stop_generation)
    }

    /// Generation whose pending per-task group tickets must not be displaced
    /// by PTRACE_INTERRUPT. SIGCONT advances the generation before publishing
    /// its notification, so cancelled tickets remain replaceable.
    pub(crate) fn current_valid_group_stop(&self) -> Option<u64> {
        (self.group_stop_pending_ptraced != 0 || self.flags.contains(SignalFlags::STOP_STOPPED))
            .then_some(self.group_stop_generation)
    }

    pub(crate) fn current_completed_group_stop(&self) -> Option<(u64, Signal)> {
        self.flags
            .contains(SignalFlags::STOP_STOPPED)
            .then_some((self.group_stop_generation, self.stop_signal))
    }

    pub(crate) fn cancel_group_stop(&mut self) {
        if self.group_stop_pending_ptraced != 0 || self.flags.intersects(SignalFlags::STOP_MASK) {
            self.advance_group_stop_generation();
            self.group_stop_pending_ptraced = 0;
        }
    }
}
