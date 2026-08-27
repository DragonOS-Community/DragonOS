use super::{
    relation::{
        is_ptraced_locked, link_relation_locked, pop_tracee_locked, ptracer_of_locked,
        relation_slot_available_locked, reserve_relation_slot, unlink_relation_locked,
    },
    validate_ptrace_options, PtraceAccessCreds, PtraceFreezeToken, PtraceOptions,
    PtraceReleaseOutcome,
};
use crate::{
    arch::ipc::signal::{SigChildCode, SigFlags, Signal},
    ipc::{
        sighand::{InnerSigHand, ReapTransition},
        signal_types::{SigCode, SigInfo, SigType, SignalFlags},
    },
    process::{
        abi::WaitOption, cred, pid::PidType, ExitState, ProcessControlBlock, ProcessFlags,
        ProcessManager, ProcessState, RawPid, PTRACE_RELATION_LOCK,
    },
};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

fn traceme_allowed(
    parent: &Arc<ProcessControlBlock>,
    child: &Arc<ProcessControlBlock>,
) -> Result<(), SystemError> {
    if is_ptraced_locked(child) {
        return Err(SystemError::EPERM);
    }
    if parent.flags().contains(ProcessFlags::EXITING) {
        return Err(SystemError::EPERM);
    }
    let parent_cred = parent.cred();
    let child_cred = child.cred();
    let allowed = parent_cred
        .has_capability_in_ns(&child_cred.user_ns, cred::CAPFlags::CAP_SYS_PTRACE)
        || (Arc::ptr_eq(&parent_cred.user_ns, &child_cred.user_ns)
            && (child_cred.cap_permitted.bits() & !parent_cred.cap_permitted.bits()) == 0);
    if !allowed {
        return Err(SystemError::EPERM);
    }
    Ok(())
}

fn traceme_parent_for(
    child: &Arc<ProcessControlBlock>,
) -> Result<Arc<ProcessControlBlock>, SystemError> {
    let real_parent = child.real_parent_pcb().ok_or(SystemError::EPERM)?;
    let Some(fork_parent) = child.fork_parent_pcb() else {
        return Ok(real_parent);
    };

    if fork_parent.tgid == real_parent.tgid {
        Ok(fork_parent)
    } else {
        Ok(real_parent)
    }
}
/// Join every already-published sibling to one shared group-stop transaction.
/// The caller holds `PTRACE_RELATION_LOCK` and the shared sighand write lock;
/// fork publication takes the same locks before adding a CLONE_THREAD child.
/// Untraced siblings are committed synchronously by DragonOS' existing
/// `stop_task`, while ptraced siblings receive one generation-bound ticket and
/// complete it only after publishing their own reportable stop.
pub(super) fn arm_ptrace_group_stop_siblings_locked(
    current: &Arc<ProcessControlBlock>,
    signal: Signal,
    generation: u64,
    group: &mut InnerSigHand,
) {
    let leader = if current.is_thread_group_leader() {
        current.clone()
    } else {
        current
            .threads_read_irqsave()
            .group_leader()
            .unwrap_or_else(|| current.clone())
    };

    let mut join = |task: &Arc<ProcessControlBlock>| {
        if Arc::ptr_eq(task, current) || task.exit_state() != ExitState::Running {
            return;
        }
        if is_ptraced_locked(task) {
            let mut state = task.ptrace.state.lock_irqsave();
            if state.queue_pending_group_stop(signal, task.ptrace_session_generation(), generation)
            {
                let added = group.add_ptrace_group_stop_participant(generation);
                debug_assert!(
                    added,
                    "group-stop transaction changed while sighand was locked"
                );
            }
            task.flags().insert(ProcessFlags::PENDING_PTRACE_STOP);
            let needs_activation = state.pending_stop_needs_wake();
            drop(state);

            if needs_activation {
                if task.sched_info().state().is_stopped() {
                    let _ = ProcessManager::wakeup_stop_relation_locked(task);
                } else if task.sched_info().state().is_blocked_interruptable() {
                    let _ = ProcessManager::wakeup(task);
                }
                ProcessManager::kick(task);
            }
        } else {
            let _ = ProcessManager::stop_task(task);
        }
    };

    join(&leader);
    let threads = leader.threads_read_irqsave();
    for sibling in threads.group_tasks().iter().filter_map(Weak::upgrade) {
        join(&sibling);
    }
}

/// Join an unpublished CLONE_THREAD child while fork holds
/// membership -> relation. An untraced child can be born scheduler-stopped;
/// an inherited ptrace child must run only far enough to publish its own
/// reportable stop, so it receives one counted pending ticket instead.
pub(crate) fn join_new_thread_group_stop_locked(child: &Arc<ProcessControlBlock>) {
    child.sighand().with_group_stop_state(|group| {
        let Some(generation) = group.current_incomplete_group_stop() else {
            let Some((generation, signal)) = group.current_completed_group_stop() else {
                return;
            };
            if is_ptraced_locked(child) {
                let mut state = child.ptrace.state.lock_irqsave();
                state.queue_completed_group_stop(
                    signal,
                    child.ptrace_session_generation(),
                    generation,
                );
                child.flags().insert(ProcessFlags::PENDING_PTRACE_STOP);
            } else {
                child.sched_info().set_state(ProcessState::Stopped);
            }
            return;
        };
        if is_ptraced_locked(child) {
            let signal = group.stop_signal;
            let mut state = child.ptrace.state.lock_irqsave();
            if state.queue_pending_group_stop(signal, child.ptrace_session_generation(), generation)
            {
                let added = group.add_ptrace_group_stop_participant(generation);
                debug_assert!(added, "new thread joined a stale group-stop transaction");
            }
            child.flags().insert(ProcessFlags::PENDING_PTRACE_STOP);
        } else {
            child.sched_info().set_state(ProcessState::Stopped);
        }
    });
}

fn group_has_ptraced_locked(leader: &Arc<ProcessControlBlock>) -> bool {
    is_ptraced_locked(leader)
        || leader
            .threads_read_irqsave()
            .group_tasks()
            .iter()
            .filter_map(Weak::upgrade)
            .any(|task| is_ptraced_locked(&task))
}

/// Read-only routing check for stop-signal preparation. A mixed thread group
/// must defer the stop to tracee context so ptraced members can publish their
/// required reports instead of being hidden by the eager untraced fast path.
pub(crate) fn thread_group_has_ptraced(leader: &Arc<ProcessControlBlock>) -> bool {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    group_has_ptraced_locked(leader)
}

/// Start the same generation/count transaction from an untraced delivery
/// thread when another live member is ptraced. `None` leaves a fully-untraced
/// group on the existing lightweight transition_group_stop path.
pub(crate) fn stop_mixed_ptrace_group(
    current: &Arc<ProcessControlBlock>,
    signal: Signal,
) -> Option<bool> {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    let leader = if current.is_thread_group_leader() {
        current.clone()
    } else {
        current
            .threads_read_irqsave()
            .group_leader()
            .unwrap_or_else(|| current.clone())
    };
    if !group_has_ptraced_locked(&leader) {
        return None;
    }

    Some(current.sighand().with_group_stop_state(|group| {
        let Some(generation) = group.begin_ptrace_group_stop(signal) else {
            return false;
        };
        arm_ptrace_group_stop_siblings_locked(current, signal, generation, group);
        if ProcessManager::mark_stop().is_err() {
            group.cancel_group_stop();
            return false;
        }
        group.complete_ptrace_group_stop(generation)
    }))
}
pub(crate) enum PtraceZombieClaim {
    Claimed { need_cascade: bool },
    Blocked,
    Lost,
}

/// Atomically validate wait ownership, claim the zombie, and unlink the
/// ptrace relation. This mirrors Linux's EXIT_ZOMBIE -> EXIT_TRACE transition
/// while tasklist_lock still protects the relationship.
pub(crate) fn claim_and_unlink_wait_zombie(
    tracee: &Arc<ProcessControlBlock>,
    waiter: &Arc<ProcessControlBlock>,
    options: WaitOption,
) -> PtraceZombieClaim {
    let relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    let Some(tracer) = ptracer_of_locked(tracee) else {
        return PtraceZombieClaim::Lost;
    };
    let same_waiter = Arc::ptr_eq(&tracer, waiter);
    let same_thread_group = !options.contains(WaitOption::WNOTHREAD) && tracer.tgid == waiter.tgid;
    if !same_waiter && !same_thread_group {
        return PtraceZombieClaim::Lost;
    }

    match tracee.sighand().try_claim_ptraced_child(tracee) {
        ReapTransition::Blocked => return PtraceZombieClaim::Blocked,
        ReapTransition::TraceClaimed => {}
        _ => return PtraceZombieClaim::Lost,
    }

    let need_cascade = tracee
        .real_parent_pcb()
        .map(|real_parent| tracer.raw_tgid() != real_parent.raw_tgid())
        .unwrap_or(false);
    let owner = unlink_relation_locked(tracee);
    debug_assert!(
        owner
            .as_ref()
            .map(|owner| Arc::ptr_eq(owner, &tracer))
            .unwrap_or(false),
        "ptrace zombie owner changed while relation lock was held"
    );

    tracee.flags().remove(
        ProcessFlags::TRACE_SYSCALL
            | ProcessFlags::TRACE_SINGLESTEP
            | ProcessFlags::TRACE_SYSEMU
            | ProcessFlags::PT_SEIZED
            | ProcessFlags::PTRACE_EVENT_STOP
            | ProcessFlags::TRAPPING,
    );
    let (reset, group_stop_completion) = tracee.sighand().with_group_stop_state(|group| {
        let mut ps = tracee.ptrace.state.lock_irqsave();
        let reset = ps.reset_session_stop();
        tracee.flags().remove(ProcessFlags::PENDING_PTRACE_STOP);
        ps.options = PtraceOptions::empty();
        let (_, completion) = settle_reset_group_stop_locked(tracee, &reset, None, group);
        (reset, completion)
    });
    drop(relation_guard);
    reset.freeze_release.apply(tracee);
    if let Some(signal) = group_stop_completion {
        tracee.notify_natural_group_stop(signal);
    }

    PtraceZombieClaim::Claimed { need_cascade }
}

pub fn traceme_current() -> Result<(), SystemError> {
    let current = ProcessManager::current_pcb();
    loop {
        let reserve_for = {
            let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
            let tracer = traceme_parent_for(&current)?;
            traceme_allowed(&tracer, &current)?;
            if relation_slot_available_locked(&tracer) {
                link_relation_locked(&current, &tracer, PtraceOptions::empty(), false, None)?;
                break;
            }
            tracer
        };
        reserve_relation_slot(&reserve_for)?;
    }
    // The relation lock has been released with the block above: if this process carries an EXITKILL
    // verdict left by its old tracer's exit, take it over and execute it here (must not send while holding the lock, see carry_out_pending_exitkill).
    carry_out_pending_exitkill(&current);
    Ok(())
}

pub fn unlink_tracee(tracee: &Arc<ProcessControlBlock>) {
    let (tracer, release, group_stop_completion) = {
        let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
        let tracer = unlink_relation_locked(tracee);
        let (release, completion) = tracee.sighand().with_group_stop_state(|group| {
            let mut state = tracee.ptrace.state.lock_irqsave();
            let reset = state.reset_session_stop();
            tracee.flags().remove(ProcessFlags::PENDING_PTRACE_STOP);
            let (_, completion) = settle_reset_group_stop_locked(tracee, &reset, None, group);
            (reset.freeze_release, completion)
        });
        (tracer, release, completion)
    };
    release.apply(tracee);
    if let Some(signal) = group_stop_completion {
        tracee.notify_natural_group_stop(signal);
    }

    // Linux wakes the ptrace parent before destroying the old leader in
    // de_thread().  DragonOS keeps separate per-task wait queues, so both the
    // tracer and natural parent must recheck their wait ownership after the
    // relation and index update become visible.
    if let Some(tracer) = tracer.as_ref() {
        ProcessManager::wake_wait_parent(tracer);
    }

    if let Some(real_parent) = tracee.real_parent_pcb() {
        if !tracer
            .as_ref()
            .map(|tracer| Arc::ptr_eq(tracer, &real_parent))
            .unwrap_or(false)
        {
            ProcessManager::wake_wait_parent(&real_parent);
        }
    }
}

/// An exiting thread can no longer run its generation-bound pending stop.
/// Retire that one ticket before Zombie publication so a live sibling can be
/// the real last completer rather than waiting for this task to be reaped.
pub(crate) fn settle_exiting_group_stop(tracee: &Arc<ProcessControlBlock>) {
    let completion = {
        let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
        tracee.sighand().with_group_stop_state(|group| {
            let pending = {
                let mut state = tracee.ptrace.state.lock_irqsave();
                let pending = state.take_pending_stop();
                tracee.flags().remove(ProcessFlags::PENDING_PTRACE_STOP);
                pending
            };
            match pending.map(|pending| (pending.kind, pending.signal)) {
                Some((
                    super::PendingStopKind::Group {
                        generation,
                        counted: true,
                    },
                    signal,
                )) if group.complete_ptrace_group_stop(generation) => Some(signal),
                _ => None,
            }
        })
    };
    if let Some(signal) = completion {
        tracee.notify_natural_group_stop(signal);
    }
}

/// Per-tracee side-effect snapshot: phase one collects it while holding the lock, phase two executes it outside the lock when a tracer exits/is destroyed.
struct ExitPtracePending {
    tracee: Arc<ProcessControlBlock>,
    /// Deferred fatal wake taken over while the old session is revoked.
    freeze_release: PtraceReleaseOutcome,
    /// Whether SIGKILL must be sent to the tracee (the old session set PTRACE_O_EXITKILL).
    exitkill: bool,
    /// Whether session teardown removed a scheduler stop owned by ptrace.
    had_ptrace_stop: bool,
    /// real_parent kept alive past the lock via an Arc clone taken while holding the lock.
    real_parent: Option<Arc<ProcessControlBlock>>,
    group_stop_completion: Option<Signal>,
}

/// Lock-free second phase of one explicit/rollback unlink transaction.
/// The ordinary scheduler transition is already committed by phase one; only
/// a fatal signal wake, which may enter signal delivery, remains deferred.
struct PtraceUnlinkPending {
    tracee: Arc<ProcessControlBlock>,
    freeze_release: PtraceReleaseOutcome,
    group_stop_completion: Option<Signal>,
}

impl PtraceUnlinkPending {
    fn finish(self) {
        self.freeze_release.apply(&self.tracee);
        if let Some(signal) = self.group_stop_completion {
            self.tracee.notify_natural_group_stop(signal);
        }
    }
}

/// Settle the group-stop ownership removed by one ptrace session reset. The
/// caller holds relation -> shared sighand; scheduler state is committed before
/// the last pending participant publishes STOP_STOPPED.
fn settle_reset_group_stop_locked(
    tracee: &Arc<ProcessControlBlock>,
    reset: &super::PtraceSessionResetOutcome,
    resumed_group_stop: Option<u64>,
    group: &mut InnerSigHand,
) -> (bool, Option<Signal>) {
    let active_group_stop = resumed_group_stop.or(reset.active_group_stop);
    let preserve_active = active_group_stop
        .map(|generation| group.ptrace_group_stop_is_current(generation))
        .unwrap_or(false);
    let mut completed = None;
    if let Some((generation, signal, counted)) = reset.pending_group_stop {
        let current = if counted {
            group.ptrace_group_stop_in_progress(generation)
        } else {
            group.ptrace_group_stop_is_current(generation)
        };
        if current {
            if tracee.exit_state() == ExitState::Running {
                let _ = ProcessManager::stop_task(tracee);
            }
            if counted && group.complete_ptrace_group_stop(generation) {
                completed = Some(signal);
            }
        }
    }
    (
        preserve_active || group.flags.contains(SignalFlags::STOP_STOPPED),
        completed,
    )
}

/// Consume the tracee's EXITKILL doom bit (read and clear).
/// The caller must already hold `PTRACE_RELATION_LOCK`: consuming the bit and mutating the relation state are
/// mutually exclusive within the same critical section, guaranteeing exactly one consumer obtains the doom bit.
fn consume_exitkill_doom_locked(tracee: &ProcessControlBlock) -> bool {
    let mut ps = tracee.ptrace.state.lock_irqsave();
    ps.take_exitkill_verdict()
}

/// Take over and execute an EXITKILL verdict left by the old session.
/// Called after a new tracing relation is established (attach/seize/traceme succeeds) or after an attach
/// failure rolls back: if the tracee carries a doom bit verdict from its old tracer's exit, consume it here and
/// send SIGKILL -- corresponding to Linux attaching to an already SIGKILL-pending task: attach succeeds, and the
/// task then dies. Must be called outside `PTRACE_RELATION_LOCK` (it acquires the lock itself; the SIGKILL send path
/// involves memory allocation and scheduler locks, so it cannot run inside an IRQ-disabled spinlock critical section).
fn carry_out_pending_exitkill(tracee: &ProcessControlBlock) {
    let doomed = {
        let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
        consume_exitkill_doom_locked(tracee)
    };
    if !doomed {
        return;
    }
    if let Some(strong) = tracee.self_ref.upgrade() {
        let _ = Signal::SIGKILL.send_signal_info_to_pcb(None, strong, PidType::PID);
    }
}

/// Tear down the tracing relation for all tracees when a tracer exits/is destroyed.
pub fn exit_ptrace(tracer: &Arc<ProcessControlBlock>) {
    // Pop one relation per transaction.  Unlike the old `mem::take + Vec`
    // snapshot this is an allocation-free, O(1)-per-tracee exit path.
    loop {
        let pending = {
            let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
            let Some(tracee) = pop_tracee_locked(tracer) else {
                break;
            };

            // Clear the syscall-trace/single-step working bits to avoid leftovers.
            tracee.flags().remove(
                ProcessFlags::TRACE_SYSCALL
                    | ProcessFlags::TRACE_SINGLESTEP
                    | ProcessFlags::TRACE_SYSEMU
                    | ProcessFlags::PT_SEIZED,
            );

            // Unconditionally clear the single-step TF
            // A running tracee may have TF set via PTRACE_SINGLESTEP/SYSEMU_SINGLESTEP;
            // if the tracer exits without clearing it, the tracee will hit #DB after resuming and force_sig(SIGTRAP) kills it.
            tracee.disable_single_step();

            // Serialize the group-stop decision with SIGCONT.  Keep this guard
            // through ptrace-state reset and any scheduler wake so SIGCONT
            // cannot observe half of the teardown transaction.
            let sighand = tracee.sighand();
            // Clear the ptrace-side state and settle its group-stop ticket in
            // the same shared-sighand transaction as SIGCONT.
            let (
                exitkill,
                had_ptrace_stop,
                group_stop_active,
                freeze_release,
                group_stop_completion,
            ) = sighand.with_group_stop_state(|group| {
                let mut ps = tracee.ptrace.state.lock_irqsave();
                // Capture EXITKILL from the same state snapshot that is reset.
                let exitkill = ps.options.contains(PtraceOptions::EXITKILL);
                let reset = ps.reset_session_stop();
                tracee.flags().remove(ProcessFlags::PENDING_PTRACE_STOP);
                // Clear the ptrace options, symmetric to ptrace_unlink: prevents the tracee from
                // inheriting this session's leftover options (EXITKILL, etc.) after being re-attached by a new tracer.
                ps.options = PtraceOptions::empty();
                // The EXITKILL verdict and the relation teardown are published in the same critical
                // section: once the doom bit is set, the right to send SIGKILL goes to whoever consumes
                // that bit (phase two, or a subsequent attach/seize/traceme that establishes a new relation).
                if exitkill {
                    ps.publish_exitkill_verdict();
                }
                let (group_stop_active, group_stop_completion) = if exitkill {
                    group.cancel_group_stop();
                    (false, None)
                } else {
                    settle_reset_group_stop_locked(&tracee, &reset, None, group)
                };
                (
                    exitkill,
                    reset.had_active_stop && tracee.sched_info().state().is_stopped(),
                    group_stop_active,
                    reset.freeze_release,
                    group_stop_completion,
                )
            });
            tracee
                .flags()
                .remove(ProcessFlags::PTRACE_EVENT_STOP | ProcessFlags::TRAPPING);

            if had_ptrace_stop && !group_stop_active && !exitkill && !freeze_release.fatal_wake {
                // Relation removal, active-stop reset and Stopped -> Runnable
                // are one transaction.  A new attach cannot publish a stop
                // between the orphan decision and this scheduler commit.
                let _ = ProcessManager::wakeup_stop_relation_locked(&tracee);
            }
            // Take one Arc clone of real_parent while holding the lock, keeping it alive for use outside the lock.
            let real_parent = tracee.real_parent_pcb();

            ExitPtracePending {
                tracee,
                freeze_release,
                exitkill,
                had_ptrace_stop,
                real_parent,
                group_stop_completion,
            }
        };

        // Phase two: execute SIGKILL and fatal-signal side effects after leaving PTRACE_RELATION_LOCK.
        // Note: between phase1 clearing the relation and phase2 executing, a concurrent PTRACE_ATTACH may re-attach the tracee.
        // PTRACE_RELATION_LOCK is an IRQ-disabling spinlock and cannot be held across signal delivery,
        // so the whole exit_ptrace cannot be made atomic the way Linux's tasklist_lock allows. The EXITKILL verdict and send are
        // therefore transactionalized via the doom bit: phase one sets exitkill_pending in the same critical section that clears the
        // relation, and the right to send belongs to whichever consumer obtains the bit. Consuming here additionally requires still being
        // an orphan (not taken over by a concurrent attach) -- the doom consume and the relation check complete atomically in the same
        // critical section; if it has been re-attached, the consume right is left to the attach side (carry_out_pending_exitkill),
        // and this session no longer sends, closing the mistaken-kill window.
        let ExitPtracePending {
            tracee,
            freeze_release,
            exitkill,
            had_ptrace_stop,
            real_parent,
            group_stop_completion,
        } = pending;
        freeze_release.apply(&tracee);
        if let Some(signal) = group_stop_completion {
            tracee.notify_natural_group_stop(signal);
        }
        let (still_orphan, doomed) = {
            let _g = PTRACE_RELATION_LOCK.lock_irqsave();
            let orphan = !is_ptraced_locked(&tracee);
            // Only consume the doom bit while still an orphan: a re-attached tracee is taken over by the attach side.
            let doomed = orphan && exitkill && consume_exitkill_doom_locked(&tracee);
            (orphan, doomed)
        };
        if !still_orphan {
            // Already re-traced by a concurrent ATTACH: the new tracer owns this tracee, so skip this session's side effects.
            continue;
        }
        if doomed {
            // SIGKILL cannot be blocked/ignored; the tracee will be terminated
            let _ = Signal::SIGKILL.send_signal_info_to_pcb(None, tracee.clone(), PidType::PID);
        }

        if had_ptrace_stop {
            // real_parent's wait wakeup is independent of the tracee wakeup (notify if present).
            if let Some(real_parent) = real_parent {
                ProcessManager::wake_wait_parent(&real_parent);
            }
        } else if let Some(real_parent) = real_parent {
            // Not in a ptrace-stop (e.g. running): wake the waiters (parent + leader).
            ProcessManager::wake_wait_parent(&real_parent);
        }
    }
}
impl ProcessControlBlock {
    /// Tear down the tracing relation and restore the tracee's execution state per its group-stop status:
    /// remove it from the ptraced list, clear the syscall-trace working bits, and either transition TracedStopped -> Stopped or wake it up.
    pub fn ptrace_unlink(&self) -> Result<(), SystemError> {
        let pending = {
            let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
            self.ptrace_unlink_locked(false, None)?
        };
        pending.finish();
        Ok(())
    }

    /// Commit an unlink while the caller holds `PTRACE_RELATION_LOCK`.
    fn ptrace_unlink_locked(
        &self,
        resumed_ptrace_stop: bool,
        resumed_group_stop: Option<u64>,
    ) -> Result<PtraceUnlinkPending, SystemError> {
        // Take out the tracer and clear the bidirectional relation with an O(1) swap_remove.
        let me = self.self_ref.upgrade().ok_or(SystemError::ESRCH)?;
        let _tracer = unlink_relation_locked(&me);

        // Clear the syscall-trace / single-step working bits to avoid leftovers after detach.
        #[cfg(target_arch = "x86_64")]
        self.disable_single_step();

        self.flags().remove(
            ProcessFlags::TRACE_SYSCALL
                | ProcessFlags::TRACE_SINGLESTEP
                | ProcessFlags::TRACE_SYSEMU
                | ProcessFlags::PT_SEIZED,
        );
        // Keep SIGCONT excluded from the group-stop snapshot through active
        // stop reset and the possible scheduler wake.
        let sighand = self.sighand();
        let (had_ptrace_stop, group_stop_active, freeze_release, group_stop_completion) = sighand
            .with_group_stop_state(|group| {
                let mut ps = self.ptrace.state.lock_irqsave();
                let reset = ps.reset_session_stop();
                self.flags().remove(ProcessFlags::PENDING_PTRACE_STOP);
                ps.options = PtraceOptions::empty();
                let had_ptrace_stop = (resumed_ptrace_stop || reset.had_active_stop)
                    && self.sched_info().state().is_stopped();
                let (group_stop_active, group_stop_completion) =
                    settle_reset_group_stop_locked(&me, &reset, resumed_group_stop, group);
                (
                    had_ptrace_stop,
                    group_stop_active,
                    reset.freeze_release,
                    group_stop_completion,
                )
            });
        self.flags()
            .remove(ProcessFlags::PTRACE_EVENT_STOP | ProcessFlags::TRAPPING);

        if had_ptrace_stop && !group_stop_active && !freeze_release.fatal_wake {
            // This is the unlink transaction's linearization point for
            // execution state: relation and active-stop ownership are already
            // revoked, while a reattach is still excluded by relation lock.
            let _ = ProcessManager::wakeup_stop_relation_locked(&me);
        }
        Ok(PtraceUnlinkPending {
            tracee: me,
            freeze_release,
            group_stop_completion,
        })
    }
    pub(super) fn ptrace_set_trapping(&self) {
        self.flags().insert(ProcessFlags::TRAPPING);
    }

    pub(super) fn ptrace_clear_trapping(&self) {
        let was_trapping = self.flags().test_and_clear(ProcessFlags::TRAPPING);
        if was_trapping {
            // Wake up the attach waiters
            self.wait_queue
                .wakeup_all(Some(ProcessState::Blocked(true)));
        }
    }

    /// Publish a seized-session stop while the tracing relation is stable.
    /// This is the producer-side counterpart of reset_session_stop(): unlink
    /// either runs before this transaction (and publication is rejected) or
    /// after it (and clears the payload/flag in the same relation domain).
    pub(super) fn ptrace_queue_seized_stop_bound(
        &self,
        tracer: Option<&Arc<ProcessControlBlock>>,
        signal: Signal,
    ) -> Result<bool, SystemError> {
        let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
        self.ptrace_queue_seized_stop_relation_locked(tracer, signal)
    }

    /// Publish a seized stop while the caller keeps `PTRACE_RELATION_LOCK`.
    ///
    /// SIGCONT uses this entry point for the whole cancel-and-retrap
    /// transaction, so a stop selected for one session cannot be redirected to
    /// a session installed after the group-stop cancellation.
    pub(crate) fn ptrace_queue_seized_stop_relation_locked(
        &self,
        tracer: Option<&Arc<ProcessControlBlock>>,
        signal: Signal,
    ) -> Result<bool, SystemError> {
        let current_group_stop = self
            .sighand()
            .with_group_stop_state(|group| group.current_valid_group_stop());
        let (tracee, needs_activation) =
            self.ptrace_publish_seized_stop_relation_locked(tracer, signal, current_group_stop)?;
        if needs_activation && self.sched_info().state().is_stopped() {
            // The pending record and Stopped -> Runnable transition belong to
            // the same tracing session. Keeping relation+sighand through the
            // scheduler commit serializes it with SIGCONT and detach.
            return Ok(ProcessManager::wakeup_stop_relation_locked(&tracee).is_err());
        }

        // Running/interruptible-blocked tasks need only the ordinary deferred
        // wake/kick side effect, which must remain outside the relation lock.
        Ok(needs_activation)
    }

    /// Publish only the session-bound pending record. SIGCONT uses this phase
    /// while holding the group transaction and performs scheduler activation
    /// after releasing its global IRQ-off locks.
    fn ptrace_publish_seized_stop_relation_locked(
        &self,
        tracer: Option<&Arc<ProcessControlBlock>>,
        signal: Signal,
        current_group_stop: Option<u64>,
    ) -> Result<(Arc<ProcessControlBlock>, bool), SystemError> {
        let tracee = self.self_ref.upgrade().ok_or(SystemError::ESRCH)?;
        let owner = ptracer_of_locked(&tracee).ok_or(SystemError::ESRCH)?;
        if tracer
            .map(|expected| !Arc::ptr_eq(&owner, expected))
            .unwrap_or(false)
        {
            return Err(SystemError::ESRCH);
        }
        if !self.flags().contains(ProcessFlags::PT_SEIZED) {
            return Err(SystemError::EIO);
        }
        let mut state = self.ptrace.state.lock_irqsave();
        state.queue_pending_stop(signal, self.ptrace_session_generation(), current_group_stop);
        self.flags().insert(ProcessFlags::PENDING_PTRACE_STOP);
        let needs_activation = state.pending_stop_needs_wake();
        drop(state);
        Ok((tracee, needs_activation))
    }

    /// Complete one task's SIGCONT scheduler phase in a short relation-bound
    /// transaction. A detach/reattach cannot publish a new active stop between
    /// this verdict and the Stopped -> Runnable commit.
    pub(crate) fn ptrace_activate_after_group_continue(&self) {
        let needs_deferred_activation = {
            let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
            let Some(tracee) = self.self_ref.upgrade() else {
                return;
            };

            if self.flags().contains(ProcessFlags::PENDING_PTRACE_STOP) {
                let needs_wake = self.ptrace.state.lock_irqsave().pending_stop_needs_wake();
                if needs_wake && tracee.sched_info().state().is_stopped() {
                    ProcessManager::wakeup_stop_relation_locked(&tracee).is_err()
                } else {
                    needs_wake
                }
            } else {
                // Every seized session present in the original SIGCONT
                // transaction received a generation-bound pending ticket.
                // Without one, this is a later seized session (or one whose
                // own transition already completed), so the old SIGCONT must
                // not wake its LISTEN stop.
                if self.flags().contains(ProcessFlags::PT_SEIZED) {
                    return;
                }
                let reportable_stop = self.ptrace.state.lock_irqsave().is_traced_stop();
                if !reportable_stop && tracee.sched_info().state().is_stopped() {
                    let _ = ProcessManager::wakeup_stop_relation_locked(&tracee);
                }
                false
            }
        };

        if needs_deferred_activation {
            self.ptrace_activate_pending_stop();
        }
    }

    /// The attach side waits for the tracee to complete the STOPPED -> TRACED transition (TRAPPING cleared).
    fn ptrace_wait_trapping_cleared(&self) {
        let _ = self.wait_queue.wait_event_killable(
            || !self.flags().contains(ProcessFlags::TRAPPING),
            None::<fn()>,
        );
    }

    /// If the tracee is currently in a group-stop (Stopped), queue an attach trap and wake it up to
    /// complete the STOPPED -> TRACED transition by itself. Aligns with JOBCTL_TRAP_STOP in Linux's ptrace_attach().
    fn ptrace_arm_attach_trap_if_stopped(
        &self,
        tracer: &Arc<ProcessControlBlock>,
    ) -> Result<bool, SystemError> {
        let stop_sig = self.sighand().stop_signal();

        {
            let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
            let tracee = self.self_ref.upgrade().ok_or(SystemError::ESRCH)?;
            if !ptracer_of_locked(&tracee)
                .map(|owner| Arc::ptr_eq(&owner, tracer))
                .unwrap_or(false)
            {
                return Err(SystemError::ESRCH);
            }
            let _pi = self.sched_info().pi_lock_irqsave();
            if !self.sched_info().state().is_stopped() {
                return Ok(false);
            }
            self.ptrace_set_trapping();
            let mut state = self.ptrace.state.lock_irqsave();
            state.queue_pending_stop(stop_sig, self.ptrace_session_generation(), None);
            self.flags().insert(ProcessFlags::PENDING_PTRACE_STOP);
        }
        if let Some(strong) = self.self_ref.upgrade() {
            let _ = ProcessManager::wakeup_stop(&strong);
        }
        Ok(true)
    }

    pub fn ptrace_attach(&self, tracer: &Arc<ProcessControlBlock>) -> Result<isize, SystemError> {
        let _exec_guard = self.exec_update_read();
        let is_same_thread_group = tracer.tgid == self.tgid;

        if !tracer.has_permission_to_trace(self, PtraceAccessCreds::RealCreds)
            || self.flags().contains(ProcessFlags::KTHREAD)
            || is_same_thread_group
        {
            return Err(SystemError::EPERM);
        }

        self.ptrace_link(tracer)?;
        let strong_ref = self.self_ref.upgrade().ok_or_else(|| {
            // self_ref upgrade failed: the process is being destroyed, so roll back the relation.
            let _ = self.ptrace_unlink();
            SystemError::ESRCH
        })?;

        // Non-SEIZE ATTACH:
        // If the target is already in a group-stop (Stopped), convert its group-stop into a ptrace-stop
        // directly; only send SIGSTOP to make it stop when the target is not already Stopped.
        if self.ptrace_arm_attach_trap_if_stopped(tracer)? {
            // The target is already group-stopped: wait for the tracee itself to commit the ptrace-stop and clear TRAPPING.
            self.ptrace_wait_trapping_cleared();
        } else {
            let mut info = SigInfo::new(
                Signal::SIGSTOP,
                0,
                SigCode::Kernel,
                SigType::Kill {
                    pid: RawPid(0),
                    uid: 0,
                },
            );
            if let Err(e) = Signal::SIGSTOP.send_signal_info_to_pcb(
                Some(&mut info),
                strong_ref.clone(),
                PidType::PID,
            ) {
                // Roll back on attach failure: the target may carry an EXITKILL verdict left by the old
                // session (exit_ptrace's phase two was skipped because of this link), so take it over and
                // execute it here to avoid the doom bit being stranded and never consumed.
                let _ = self.ptrace_unlink();
                carry_out_pending_exitkill(self);
                return Err(e);
            }
        }
        // After the stop protocol completes, take over any EXITKILL verdict left by the old session (if present):
        // this corresponds to Linux attaching to an already SIGKILL-pending task -- attach succeeds, and the task
        // then dies. Placed here rather than inside link to avoid interleaving the TRAPPING wait with the death.
        carry_out_pending_exitkill(self);

        Ok(0)
    }

    /// Handle PTRACE_SEIZE.
    /// Does not send SIGSTOP; sets PT_SEIZED + options.
    pub fn ptrace_seize(
        &self,
        tracer: &Arc<ProcessControlBlock>,
        options: PtraceOptions,
    ) -> Result<isize, SystemError> {
        let _exec_guard = self.exec_update_read();
        validate_ptrace_options(options)?;
        let is_same_thread_group = tracer.tgid == self.tgid;
        if !tracer.has_permission_to_trace(self, PtraceAccessCreds::RealCreds)
            || self.flags().contains(ProcessFlags::KTHREAD)
            || is_same_thread_group
        {
            return Err(SystemError::EPERM);
        }

        // Relation, PT_SEIZED and options become visible as one relation-lock
        // transaction. In particular, a concurrent fork cannot inherit a
        // half-configured SEIZE session.
        self.ptrace_link_configured(tracer, options, true)?;
        // Take over any EXITKILL verdict left by the old session (if present), same semantics as the tail of attach.
        carry_out_pending_exitkill(self);
        Ok(0)
    }

    /// Handle PTRACE_DETACH.
    pub(super) fn ptrace_detach_guarded(
        &self,
        tracer: &Arc<ProcessControlBlock>,
        token: PtraceFreezeToken,
        signal: Option<Signal>,
    ) -> Result<isize, SystemError> {
        // data=0 means no signal is injected
        let data_signal = match signal {
            None => Signal::INVALID,
            Some(s) => {
                if s == Signal::INVALID {
                    return Err(SystemError::EIO);
                }
                s
            }
        };
        let pending = {
            let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
            let tracee = self.self_ref.upgrade().ok_or(SystemError::ESRCH)?;
            if self.ptrace_session_generation() != token.session_generation
                || !ptracer_of_locked(&tracee)
                    .map(|owner| Arc::ptr_eq(&owner, tracer))
                    .unwrap_or(false)
            {
                return Err(SystemError::ESRCH);
            }
            let mut ps = self.ptrace.state.lock_irqsave();
            if !ps.freeze_owner_matches(token) {
                return Err(SystemError::ESRCH);
            }
            let resumed_group_stop = ps.active_group_stop_generation();
            ps.prepare_resume(data_signal)?;
            drop(ps);
            self.flags().remove(ProcessFlags::PTRACE_EVENT_STOP);

            // Keep validation, resume preparation, relation removal, session
            // advance and freeze revocation in one relation transaction. This
            // is the detach counterpart of Linux's tasklist_lock critical section.
            self.ptrace_unlink_locked(true, resumed_group_stop)?
        };
        pending.finish();
        Ok(0)
    }
    pub(super) fn notify_natural_group_stop(&self, stop_signal: Signal) {
        let ptracer = self.ptracer_pcb();
        self.notify_natural_group_stop_bound(stop_signal, ptracer.as_ref());
    }

    /// Notify the natural parent using the ptracer that owned the stop at its
    /// publication point. Re-reading the relation here could let tracer exit
    /// plus re-seize change the suppression decision after the stop committed.
    pub(super) fn notify_natural_group_stop_bound(
        &self,
        stop_signal: Signal,
        ptracer: Option<&Arc<ProcessControlBlock>>,
    ) {
        let subject = if self.is_thread_group_leader() {
            self.self_ref.upgrade()
        } else {
            self.threads_read_irqsave().group_leader()
        };
        let Some(subject) = subject else {
            return;
        };
        let real_parent_to_notify = match (ptracer, subject.real_parent_pcb()) {
            (Some(ptracer), Some(rp)) if ptracer.tgid != rp.tgid => Some(rp),
            (None, Some(rp)) => Some(rp),
            _ => None,
        };
        if let Some(rp) = real_parent_to_notify {
            let mut chld = SigInfo::new(
                Signal::SIGCHLD,
                0,
                SigCode::Raw(SigChildCode::Stopped as i32),
                SigType::SigChild {
                    pid: subject.raw_pid(),
                    uid: 0,
                    status: stop_signal as i32,
                    utime: 0,
                    stime: 0,
                },
            );
            // Do not send CLD_STOPPED when real_parent has set SA_NOCLDSTOP or SIG_IGN
            let send = match rp.sighand().handler(Signal::SIGCHLD) {
                Some(a) => !a.action().is_ignore() && !a.flags().contains(SigFlags::SA_NOCLDSTOP),
                None => true,
            };
            if send {
                let _ = Signal::SIGCHLD.send_signal_info_to_pcb(
                    Some(&mut chld),
                    rp.clone(),
                    PidType::TGID,
                );
            }
            rp.wake_all_waiters();
        }
    }
}

/// Apply SIGCONT's shared job-control transition and publish every seized
/// re-trap while relation ownership is stable. Scheduler activation remains a
/// separate, idempotent phase in the signal path after this function returns.
///
/// The second return value says that the transition callback ran and the
/// caller must perform the scheduler side effects after dropping both locks.
pub(crate) fn continue_group_with_ptrace(leader: &Arc<ProcessControlBlock>) -> (bool, bool) {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    let mut continue_committed = false;
    let was_stopped = leader.sighand().transition_group_continue(|| {
        continue_committed = true;

        let publish_retrap = |task: &Arc<ProcessControlBlock>| {
            if task.flags().contains(ProcessFlags::PT_SEIZED) {
                let _ =
                    task.ptrace_publish_seized_stop_relation_locked(None, Signal::SIGTRAP, None);
            }
        };
        publish_retrap(leader);

        // Borrow the leader-owned list while its read guard is alive. This is
        // an IRQ-off global transaction, so it must neither allocate a clone
        // nor enter the scheduler for every member.
        let threads = leader.threads_read_irqsave();
        for weak in threads.group_tasks() {
            let Some(task) = weak.upgrade() else {
                continue;
            };
            if !Arc::ptr_eq(&task, leader) {
                publish_retrap(&task);
            }
        }
    });
    (was_stopped, continue_committed)
}
