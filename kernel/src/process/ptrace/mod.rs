use super::{
    ExitState, ProcessControlBlock, ProcessFlags, ProcessManager, RawPid, PTRACE_RELATION_LOCK,
};
use crate::libs::spinlock::SpinLock;
use crate::{
    arch::{
        interrupt::TrapFrame,
        ipc::signal::{SigChildCode, SigFlags, Signal},
        CurrentIrqArch,
    },
    exception::InterruptArch,
    ipc::signal_types::{SigCode, SigInfo, SigType},
    process::{namespace::user_namespace::map_id_up, pid::PidType, ProcessState},
    sched::{schedule, SchedMode},
};
use alloc::sync::Arc;
use core::sync::atomic::{fence, Ordering};
use system_error::SystemError;

/// ptrace hook on the signal-delivery path: called after `do_signal` dequeues a signal, before action lookup.
pub fn ptrace_signal(
    pcb: &Arc<ProcessControlBlock>,
    original: Signal,
    info: &mut Option<SigInfo>,
) -> Option<Signal> {
    // SIGKILL never passes through ptrace_signal (defensive: do_signal already handles it on the kernel_only path).
    if original == Signal::SIGKILL {
        return Some(Signal::SIGKILL);
    }

    // Enter signal-delivery-stop. ptrace_stop internally bails out early on fatal signals, blocks, then cleans up after wakeup.
    let signr = pcb
        .ptrace_stop(
            original as usize,
            SigChildCode::Trapped,
            0,
            info.as_mut(),
            None,
            None,
        )
        .signal_result();

    if signr == 0 {
        // The tracer discarded the signal.
        return None;
    }

    let injected = Signal::from(signr as i32);
    if injected == Signal::INVALID {
        return None;
    }

    // If the tracer changed the signal number, rebuild siginfo (source SI_USER).
    if let Some(i) = info {
        if injected as i32 != i.signo_i32() {
            let sender = crate::process::ptrace::ptracer_of(pcb).or_else(|| pcb.real_parent_pcb());
            let sender_vpid = sender
                .as_ref()
                .and_then(|parent| parent.task_pid_nr_ns(PidType::PID, Some(pcb.active_pid_ns())))
                .map(|p| p.data())
                .unwrap_or(0);
            // Fall back to overflowuid (default 65534) when the cross-user-namespace mapping fails
            const OVERFLOWUID: u32 = 65534;
            let sender_uid = sender
                .as_ref()
                .map(|p| {
                    let kuid = p.cred().uid.data() as u32;
                    map_id_up(&pcb.cred().user_ns.inner.lock().uid_map, kuid).unwrap_or(OVERFLOWUID)
                })
                .unwrap_or(OVERFLOWUID);
            *i = SigInfo::new(
                injected,
                0,
                SigCode::User,
                SigType::Kill {
                    pid: RawPid(sender_vpid),
                    uid: sender_uid,
                },
            );
        }
    }

    // If the injected signal is blocked by the current mask, or a fatal signal is pending, requeue it and return None so do_signal continues dequeuing the next one
    let blocked = {
        let g = pcb.sig_info_irqsave();
        g.sig_blocked().contains(injected.into())
    };
    let fatal_pending = Signal::fatal_signal_pending(pcb);
    if blocked || fatal_pending {
        if let Some(i) = info.as_mut() {
            let _ = injected.send_signal_info_to_pcb(Some(i), pcb.clone(), PidType::PID);
        } else {
            let _ = injected.send_signal_info_to_pcb(None, pcb.clone(), PidType::PID);
        }
        return None;
    }

    Some(injected)
}

mod abi;

pub use abi::*;
mod stop;

pub use stop::*;
mod operation;

pub(crate) use operation::{PtraceRequestGuard, DR6_RESERVED, DR_CONTROL_RESERVED};
pub use operation::{X86_DR_BS, X86_DR_B_MASK, X86_EFLAGS_RF, X86_EFLAGS_TF};
mod relation;

pub use relation::*;
mod lifecycle;

pub use lifecycle::*;

/// Validate ptrace options implemented by this kernel.
///
/// Linux returns EINVAL when PTRACE_O_SUSPEND_SECCOMP is unavailable because
/// checkpoint/restore or seccomp support is not built in. DragonOS does not
/// implement that suspension mechanism yet, so both option-setting entry
/// points must reject it as unsupported rather than as a permission failure.
fn validate_ptrace_options(options: PtraceOptions) -> Result<(), SystemError> {
    if options.contains(PtraceOptions::SUSPEND_SECCOMP) {
        Err(SystemError::EINVAL)
    } else {
        Ok(())
    }
}

/// All ptrace-owned storage embedded in one task.
///
/// This is an ownership header only: it preserves the existing relation locks
/// and the single irq-safe state lock without adding another synchronization
/// layer or allocation.
#[derive(Debug)]
pub(super) struct PtraceTask {
    relations: relation::PtraceRelations,
    state: SpinLock<PtraceState>,
}

impl PtraceTask {
    pub(super) fn new() -> Self {
        Self {
            relations: relation::PtraceRelations::new(),
            state: SpinLock::new(PtraceState::new()),
        }
    }
}

/// Internal result of attempting to publish one ptrace stop.
///
/// The fallback value is consumed only by signal-delivery-style callers when
/// no stop was published. Event callers inspect the typed outcome instead of
/// inferring publication from the injected signal chosen when a real stop was
/// resumed.
#[derive(Debug, Clone, Copy)]
enum PtraceStopOutcome {
    NotCommitted { fallback_signal: usize },
    Committed { injected_signal: Signal },
}

/// Result of an option-gated ptrace event attempt.
///
/// `Disabled` is the only outcome for which EXEC may emit its legacy bare
/// SIGTRAP. `NotCommitted` means an enabled event belonged to a tracing
/// session that disappeared before its stop could be published, so callers
/// must not redirect it to a replacement tracer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PtraceEventOutcome {
    Disabled,
    NotCommitted,
    Committed,
}

impl PtraceStopOutcome {
    fn signal_result(self) -> usize {
        match self {
            Self::NotCommitted { fallback_signal } => fallback_signal,
            Self::Committed { injected_signal } if injected_signal != Signal::INVALID => {
                injected_signal as usize
            }
            Self::Committed { .. } => 0,
        }
    }
}

impl ProcessControlBlock {
    // Core stop state machine

    /// Enter a ptrace-stop
    fn ptrace_stop(
        &self,
        exit_code: usize,
        why: SigChildCode,
        event_message: usize,
        info: Option<&mut SigInfo>,
        expected_session: Option<u64>,
        expected_group_stop: Option<(u64, bool)>,
    ) -> PtraceStopOutcome {
        // 1. Disable interrupts (released only right before schedule, keeping the check and commit atomic).
        let irq = unsafe { CurrentIrqArch::save_and_disable_irq() };

        // 2. Relation check + arm TRAPPING + commit Stopped must be in the same PTRACE_RELATION_LOCK critical section to close the detach race
        let relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
        if !is_ptraced_locked(self)
            || expected_session
                .map(|session| self.ptrace_session_generation() != session)
                .unwrap_or(false)
        {
            let mut completed_group_stop = false;
            if let Some((group_generation, counted)) = expected_group_stop {
                let still_ptraced = is_ptraced_locked(self);
                self.sighand().with_group_stop_state(|group| {
                    let current = if counted {
                        group.ptrace_group_stop_in_progress(group_generation)
                    } else {
                        group.ptrace_group_stop_is_current(group_generation)
                    };
                    if !current {
                        return;
                    }
                    if still_ptraced && self.exit_state() == ExitState::Running {
                        let mut state = self.ptrace.state.lock_irqsave();
                        let signal = Signal::from((exit_code & EXITCODE_SIG_MASK) as i32);
                        if counted {
                            state.queue_pending_group_stop(
                                signal,
                                self.ptrace_session_generation(),
                                group_generation,
                            );
                        } else {
                            state.queue_completed_group_stop(
                                signal,
                                self.ptrace_session_generation(),
                                group_generation,
                            );
                        }
                        self.flags().insert(ProcessFlags::PENDING_PTRACE_STOP);
                    } else {
                        if self.exit_state() == ExitState::Running {
                            if let Some(strong) = self.self_ref.upgrade() {
                                let _ = ProcessManager::stop_task(&strong);
                            }
                        }
                        if counted {
                            completed_group_stop =
                                group.complete_ptrace_group_stop(group_generation);
                        }
                    }
                });
            }
            drop(relation_guard);
            drop(irq);
            // A generation-bound pending record may belong to a session that
            // has already detached.  It must not clear TRAPPING published by
            // a later attach.  Ordinary current-session stops retain the old
            // attach-abort cleanup behavior.
            if expected_session.is_none() {
                self.ptrace_clear_trapping();
            }
            if completed_group_stop {
                self.notify_natural_group_stop(Signal::from(
                    (exit_code & EXITCODE_SIG_MASK) as i32,
                ));
            }
            return PtraceStopOutcome::NotCommitted {
                fallback_signal: exit_code,
            };
        }
        let tracee = self
            .self_ref
            .upgrade()
            .expect("a running ptrace-stop task must retain its self reference");
        let stop_owner = ptracer_of_locked(&tracee)
            .expect("is_ptraced_locked must have a live owner under the relation lock");

        // 3. Arm TRAPPING (if attach has set it).
        self.ptrace_set_trapping();

        // 4. Fatal check + commit TRACED state.
        let mut completed_group_stop = None;
        let generation = {
            let sighand = self.sighand();
            sighand.with_group_stop_state(|group| {
                let siginfo_g = self.sig_info_irqsave();
                let fatal = siginfo_g
                    .sig_pending()
                    .signal()
                    .contains(Signal::SIGKILL.into())
                    || group
                        .shared_pending
                        .signal()
                        .contains(Signal::SIGKILL.into());
                if fatal {
                    if expected_group_stop.is_some() {
                        group.cancel_group_stop();
                    }
                    return None;
                }

                let stop_signal = Signal::from((exit_code & EXITCODE_SIG_MASK) as i32);
                let is_group_stop = why == SigChildCode::Stopped
                    && crate::ipc::signal_types::SIG_KERNEL_STOP_MASK.contains(stop_signal.into());
                let group_ticket = if let Some((expected, counted)) = expected_group_stop {
                    let current = if counted {
                        group.ptrace_group_stop_in_progress(expected)
                    } else {
                        group.ptrace_group_stop_is_current(expected)
                    };
                    if !current {
                        return None;
                    }
                    Some((expected, counted))
                } else if is_group_stop {
                    let generation = group.begin_ptrace_group_stop(stop_signal);
                    if let (Some(generation), Some(current)) = (generation, self.self_ref.upgrade())
                    {
                        arm_ptrace_group_stop_siblings_locked(
                            &current,
                            stop_signal,
                            generation,
                            group,
                        );
                    }
                    generation.map(|generation| (generation, true))
                } else {
                    None
                };

                let mut ps = self.ptrace.state.lock_irqsave();
                let mutable_siginfo = info.as_ref().map(|i| **i);
                let generation = ps.publish_stop(
                    exit_code,
                    mutable_siginfo,
                    event_message,
                    group_ticket.map(|(generation, _)| generation),
                );
                #[cfg(target_arch = "x86_64")]
                {
                    ps.stop_frame_on_syscall_stack = self.current_stop_frame_on_syscall_stack();
                }
                drop(ps);
                self.sched_info().set_state(ProcessState::Stopped);
                if let Some((group_generation, true)) = group_ticket {
                    if group.complete_ptrace_group_stop(group_generation) {
                        completed_group_stop = Some(stop_signal);
                    }
                }
                Some(generation)
                // siginfo_g and sighand write guard are dropped after set_state
            })
        };

        drop(relation_guard);

        // 5. fence(Release): ensures Stopped + exit_code are visible to the tracer before TRAPPING is cleared.
        fence(Ordering::Release);

        let Some(generation) = generation else {
            self.ptrace_clear_trapping();
            return PtraceStopOutcome::NotCommitted { fallback_signal: 0 };
        };

        // 6. Clear TRAPPING and wake the attach waiters.
        self.ptrace_clear_trapping();

        // 7. Notify the tracer and block.
        self.notify_tracer(&stop_owner, why, exit_code);
        // real_parent CLD_STOPPED notification: only when group-stop completes && ptracer != real_parent.
        if let Some(stop_signal) = completed_group_stop {
            self.notify_natural_group_stop_bound(stop_signal, Some(&stop_owner));
        }
        schedule(SchedMode::SM_NONE);
        // 8. Cleanup after wakeup.
        let mut ps = self.ptrace.state.lock_irqsave();
        let (saved_siginfo, injected) = ps.finish_waiter(generation);
        if let Some(i) = info {
            if let Some(saved) = saved_siginfo {
                // Refill: modifications from PTRACE_SETSIGINFO participate in subsequent signal delivery.
                *i = saved;
            }
        }
        // Only clean up the control bits if this generation is still the active stop; if another CPU
        // has already published a new-generation stop, the old waiter must not disturb the new stop's gating.
        let newer_stop = ps.has_active_stop();
        let release = if !newer_stop {
            ps.revoke_freeze_owner()
        } else {
            PtraceReleaseOutcome::default()
        };
        drop(ps);
        if let Some(strong) = self.self_ref.upgrade() {
            release.apply(&strong);
        }

        // Recompute signal pending after wakeup (the tracer may have injected a signal).
        if let Some(strong) = self.self_ref.upgrade() {
            strong.recalc_sigpending();
        }

        PtraceStopOutcome::Committed {
            injected_signal: injected,
        }
    }

    /// Typed entry point for the group-stop path, so callers don't assemble the internal stop reason.
    pub(crate) fn ptrace_group_stop(&self, signal: Signal) -> usize {
        self.ptrace_stop(signal as usize, SigChildCode::Stopped, 0, None, None, None)
            .signal_result()
    }

    /// Consume a pending ptrace trap in the tracee context.
    /// Returns true if it has been handled; the caller should continue to re-verify the sticky pending bit.
    pub fn ptrace_handle_pending_stop(&self) -> bool {
        if !self.flags().contains(ProcessFlags::PTRACED)
            || !self.flags().contains(ProcessFlags::PENDING_PTRACE_STOP)
        {
            return false;
        }
        let pending_sig = {
            let mut ps = self.ptrace.state.lock_irqsave();
            let pending = ps.take_pending_stop();
            // Clear the fast flag while the same lock excludes all producers.
            // A producer that arrives later publishes both a new payload and a
            // new flag, so repeated INTERRUPT/NOTIFY requests coalesce without
            // manufacturing an extra default-SIGTRAP stop.
            self.flags().remove(ProcessFlags::PENDING_PTRACE_STOP);
            pending
        };
        let Some(pending) = pending_sig else {
            return false;
        };
        let group_ticket = match pending.kind {
            PendingStopKind::Notify => None,
            PendingStopKind::Group {
                generation,
                counted,
            } => Some((generation, counted)),
        };
        if self.flags().contains(ProcessFlags::PT_SEIZED) {
            let _ = self.ptrace_event_stop_bound(
                pending.signal,
                Some(pending.session_generation),
                group_ticket,
            );
        } else {
            // A plain ATTACH group-stop from Linux's do_jobctl_trap() has no siginfo
            // and ignores the resume data.
            let _ = self.ptrace_stop(
                pending.signal as usize,
                SigChildCode::Stopped,
                0,
                None,
                Some(pending.session_generation),
                group_ticket,
            );
        }
        true
    }

    /// Send SIGCHLD + wake the tracer's wait_queue.
    fn notify_tracer(
        &self,
        tracer: &Arc<ProcessControlBlock>,
        why: SigChildCode,
        stop_code: usize,
    ) {
        // 1. Send SIGCHLD to the tracer (if the tracer does not ignore it).
        let should_send = {
            let sa = tracer.sighand().handler(Signal::SIGCHLD);
            match sa {
                Some(a) => !a.action().is_ignore() && !a.flags().contains(SigFlags::SA_NOCLDSTOP),
                None => false,
            }
        };
        if should_send {
            let status = match why {
                SigChildCode::Stopped | SigChildCode::Trapped => {
                    (stop_code & EXITCODE_SIG_MASK) as i32
                }
                _ => Signal::SIGCONT as i32,
            };
            let mut chld = SigInfo::new(
                Signal::SIGCHLD,
                0,
                SigCode::Raw(why as i32),
                SigType::SigChild {
                    pid: self.raw_pid(),
                    uid: 0,
                    status,
                    utime: 0,
                    stime: 0,
                },
            );
            let _ = Signal::SIGCHLD.send_signal_info_to_pcb(
                Some(&mut chld),
                tracer.clone(),
                PidType::TGID,
            );
        }
        // Unconditionally wake the ptracer's wait_queue.
        // gdb/strace do not install a SIGCHLD handler by default and block on waitpid(2); this wakeup is
        // their only reliable path to observe a ptrace-stop (the SIGCHLD above only serves signal-driven tracers).
        tracer.wake_all_waiters();
        // Also wake the group leader when it differs from the ptracer
        let leader = tracer
            .thread
            .read_irqsave()
            .group_leader()
            .unwrap_or_else(|| tracer.clone());
        if !Arc::ptr_eq(&leader, tracer) {
            leader.wake_all_waiters();
        }
    }

    /// ptrace event notification (FORK/CLONE/VFORK/EXEC/EXIT/SECCOMP).
    /// Distinguishes a disabled option, a session-lost stop, and a committed
    /// event so callers cannot redirect an old session's event.
    pub(crate) fn ptrace_event(&self, event: PtraceEvent, message: usize) -> PtraceEventOutcome {
        let Some(option) = Self::ptrace_event_option(event) else {
            return PtraceEventOutcome::Disabled;
        };
        let session_generation = {
            let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
            if !is_ptraced_locked(self)
                || !self.ptrace.state.lock_irqsave().options.contains(option)
            {
                return PtraceEventOutcome::Disabled;
            }
            self.ptrace_session_generation()
        };
        let exit_code = (event as usize) << EXITCODE_EVENT_SHIFT | Signal::SIGTRAP as usize;
        // Bind the option decision to the session that made it. Tracer exit
        // followed by re-seize must not redirect this event to the new owner.
        match Self::ptrace_notify_with_message_bound(
            exit_code,
            exit_code as i32,
            message,
            Some(session_generation),
        ) {
            Ok(PtraceStopOutcome::Committed { .. }) => PtraceEventOutcome::Committed,
            Ok(PtraceStopOutcome::NotCommitted { .. }) | Err(_) => PtraceEventOutcome::NotCommitted,
        }
    }

    /// Report a fork-family event only to the tracing session that atomically
    /// inherited the child.  `ptrace_stop` revalidates the generation while
    /// holding the relation lock, so detach/reattach cannot redirect the event.
    pub(crate) fn ptrace_fork_event_bound(
        &self,
        event: PtraceEvent,
        message: usize,
        session_generation: u64,
    ) {
        let exit_code = (event as usize) << EXITCODE_EVENT_SHIFT | Signal::SIGTRAP as usize;
        let _ = Self::ptrace_notify_with_message_bound(
            exit_code,
            exit_code as i32,
            message,
            Some(session_generation),
        );
    }

    fn ptrace_event_option(event: PtraceEvent) -> Option<PtraceOptions> {
        Some(match event {
            PtraceEvent::Fork => PtraceOptions::TRACEFORK,
            PtraceEvent::VFork => PtraceOptions::TRACEVFORK,
            PtraceEvent::Clone => PtraceOptions::TRACECLONE,
            PtraceEvent::Exec => PtraceOptions::TRACEEXEC,
            PtraceEvent::VForkDone => PtraceOptions::TRACEVFORKDONE,
            PtraceEvent::Exit => PtraceOptions::TRACEEXIT,
            PtraceEvent::Seccomp => PtraceOptions::TRACESECCOMP,
            _ => return None,
        })
    }

    /// Construct PTRACE_EVENT_STOP (seize-mode group-stop / INTERRUPT / LISTEN re-trap).
    pub(crate) fn ptrace_event_stop(&self, signal: Signal) -> usize {
        self.ptrace_event_stop_bound(signal, None, None)
    }

    fn ptrace_event_stop_bound(
        &self,
        signal: Signal,
        expected_session: Option<u64>,
        expected_group_stop: Option<(u64, bool)>,
    ) -> usize {
        let exit_code = (PtraceEvent::Stop as usize) << EXITCODE_EVENT_SHIFT | signal as usize;
        // si_code uses exit_code, so GETSIGINFO reads (Stop<<8)|signal.
        let mut info = SigInfo::new(
            signal,
            0,
            SigCode::Raw(exit_code as i32),
            SigType::Kill {
                pid: RawPid(0),
                uid: 0,
            },
        );
        self.ptrace_stop(
            exit_code,
            SigChildCode::Stopped,
            0,
            Some(&mut info),
            expected_session,
            expected_group_stop,
        )
        .signal_result()
    }

    /// Called on the SIGCONT delivery path to make a seized tracee leave group-stop/LISTEN and re-enter PTRACE_EVENT_STOP.
    pub fn ptrace_trap_notify(&self) {
        // Atomically publish the pending payload/flag and read the wakeup gate in one lock hold:
        // only the LISTEN or group-stop (non-ptrace-stop) states need wakeup_stop to re-trap;
        // a normal ptrace-stop is left alone, with PENDING re-checked after CONT in do_signal_or_restart.
        // Relation locking also prevents a delayed SIGCONT producer from
        // publishing an old-session stop after detach/reset has completed.
        let Ok(needs_deferred_wake) = self.ptrace_queue_seized_stop_bound(None, Signal::SIGTRAP)
        else {
            return;
        };
        if needs_deferred_wake {
            self.ptrace_activate_pending_stop();
        }
    }

    /// Execute only the scheduler side effect of an already-published pending
    /// stop. Session validation and payload publication happen elsewhere.
    pub(crate) fn ptrace_activate_pending_stop(&self) {
        if !self.flags().contains(ProcessFlags::PENDING_PTRACE_STOP) {
            return;
        }
        if let Some(strong) = self.self_ref.upgrade() {
            if strong.sched_info().state().is_blocked_interruptable() {
                let _ = ProcessManager::wakeup(&strong);
            }
            ProcessManager::kick(&strong);
        }
    }

    /// PTRACE_INTERRUPT: make a running SEIZED tracee enter a ptrace-stop.
    pub fn ptrace_interrupt(&self, tracer: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        // Atomically publish the pending payload/flag and read the wakeup gate in one lock hold, avoiding a TOCTOU
        // between multiple lock acquisitions. Only a Stopped tracee in the LISTEN or group-stop (non-ptrace-stop)
        // state needs wakeup_stop to re-trap; a normal ptrace-stop is left alone, with PENDING re-checked after CONT in do_signal_or_restart.
        // Ownership and PT_SEIZED are revalidated in the same relation-bound
        // transaction, so exit/unlink cannot turn this into a new-session stop.
        let needs_deferred_wake =
            self.ptrace_queue_seized_stop_bound(Some(tracer), Signal::SIGTRAP)?;
        if needs_deferred_wake {
            self.ptrace_activate_pending_stop();
        }
        Ok(())
    }

    /// PTRACE_LISTEN: make a tracee in PTRACE_EVENT_STOP leave its ptrace-stop but stay stopped.
    /// Semantics: the tracee neither runs (stays Stopped) nor is in a ptrace-stop (invisible to wait, ptrace
    /// commands fail with ESRCH); under a group-stop-originated LISTEN signals queue but are not delivered, and SIGCONT makes it leave the group-stop and re-trap.
    fn ptrace_listen_guarded(&self, token: PtraceFreezeToken) -> Result<isize, SystemError> {
        // PT_SEIZED
        if !self.flags().contains(ProcessFlags::PT_SEIZED) {
            return Err(SystemError::EIO);
        }
        let (_stop_signal, retrap, release) =
            self.ptrace.state.lock_irqsave().enter_listening(token)?;
        // A group-stop is published by the last generation-bound participant,
        // not by LISTEN on whichever tracee the tracer happens to resume first.
        if retrap && !release.fatal_wake {
            // Wake the tracee out of ptrace_stop's schedule so it consumes PENDING_PTRACE_STOP on the
            // return-to-user path and re-enters PTRACE_EVENT_STOP.
            if let Some(strong) = self.self_ref.upgrade() {
                let _ = ProcessManager::wakeup_stop(&strong);
            }
        }
        if let Some(strong) = self.self_ref.upgrade() {
            release.apply(&strong);
        }
        Ok(0)
    }

    /// Send a ptrace notification stop (exit_code must be a SIGTRAP encoding).
    /// `si_code` is written into siginfo (e.g. TRAP_BRKPT/TRAP_TRACE/TRAP_HWBKPT, or an EVENT_STOP encoding).
    /// Generic trap entry with no event message.
    pub fn ptrace_notify(exit_code: usize, si_code: i32) -> Result<usize, SystemError> {
        Self::ptrace_notify_with_message(exit_code, si_code, 0)
    }

    fn ptrace_notify_with_message(
        exit_code: usize,
        si_code: i32,
        event_message: usize,
    ) -> Result<usize, SystemError> {
        Self::ptrace_notify_with_message_bound(exit_code, si_code, event_message, None)
            .map(PtraceStopOutcome::signal_result)
    }

    fn ptrace_notify_with_message_bound(
        exit_code: usize,
        si_code: i32,
        event_message: usize,
        expected_session: Option<u64>,
    ) -> Result<PtraceStopOutcome, SystemError> {
        let current = ProcessManager::current_pcb();
        if (exit_code & (0x7f | !0xffff)) != Signal::SIGTRAP as usize {
            return Err(SystemError::EINVAL);
        }
        let mut info = SigInfo::new(
            Signal::SIGTRAP,
            0,
            SigCode::Raw(si_code),
            SigType::Kill {
                pid: current.raw_pid(),
                uid: 0,
            },
        );
        let outcome = current.ptrace_stop(
            exit_code,
            SigChildCode::Trapped,
            event_message,
            Some(&mut info),
            expected_session,
            None,
        );
        Ok(outcome)
    }

    /// Re-inject the signal returned by ptrace_stop (if non-zero).
    pub fn reinject_ptrace_signal(signr: usize) {
        if signr == 0 {
            return;
        }
        let sig = Signal::from(signr as i32);
        if sig == Signal::INVALID {
            return;
        }
        let current = ProcessManager::current_pcb();
        let _ = sig.send_signal_info_to_pcb(None, current, PidType::PID);
    }

    /// Clear RFLAGS.TF that the debugger set for single-step
    #[cfg(target_arch = "x86_64")]
    fn disable_single_step(&self) {
        // Hold the lock throughout: clearing the flag, re-verification, and writing the frame are done in the same critical section
        let mut ps = self.ptrace.state.lock_irqsave();
        if !ps.forced_trap_flag {
            return;
        }
        ps.forced_trap_flag = false;
        // Only write frame.rflags while the tracee is still in a ptrace-stop (TrapFrame stable);
        // for a running tracee stop_frame_on_syscall_stack is a stale value, and writing it is both
        // ineffective and races with this CPU's entry path concurrently writing rflags.
        if Self::trap_frame_stable_locked(self, &ps) {
            // SAFETY: The re-verification passed; the frame is stable.
            let frame = unsafe { &mut *self.trap_frame_ptr_for(ps.stop_frame_on_syscall_stack) };
            frame.rflags &= !X86_EFLAGS_TF; // clear X86_EFLAGS_TF
        }
    }

    /// Set RFLAGS.TF after re-verifying under the ptrace_state lock that the tracee is still in a
    /// ptrace-stop, and record forced_trap_flag. A fatal signal may wake the tracee after the request
    /// is validated; once the stop frame is stale, refuse to write.
    #[cfg(target_arch = "x86_64")]
    fn arm_trap_flag_single_step(&self) -> Result<(), SystemError> {
        let mut ps = self.ptrace.state.lock_irqsave();
        if !Self::trap_frame_stable_locked(self, &ps) {
            return Err(SystemError::ESRCH);
        }
        // SAFETY: The re-verification passed; the frame is stable.
        let frame = unsafe { &mut *self.trap_frame_ptr_for(ps.stop_frame_on_syscall_stack) };
        let user_tf = frame.rflags & X86_EFLAGS_TF != 0 && !ps.forced_trap_flag;
        frame.rflags |= X86_EFLAGS_TF;
        ps.forced_trap_flag = !user_tf;
        Ok(())
    }

    /// Linux x86 get_signal() handoff: stop hardware single-step before
    /// constructing a signal frame. The caller reports an immediate ptrace
    /// SIGTRAP only after frame construction succeeds.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn prepare_single_step_signal_delivery(&self, frame: &mut TrapFrame) -> bool {
        if !self.flags().contains(ProcessFlags::TRACE_SINGLESTEP) {
            return false;
        }
        self.flags().remove(ProcessFlags::TRACE_SINGLESTEP);
        let mut ps = self.ptrace.state.lock_irqsave();
        if ps.forced_trap_flag {
            frame.rflags &= !X86_EFLAGS_TF;
            ps.forced_trap_flag = false;
        }
        true
    }

    /// Non-x86_64 architectures have no hardware single-step mechanism, so clearing single-step is a no-op.
    #[cfg(not(target_arch = "x86_64"))]
    #[inline]
    fn disable_single_step(&self) {}

    /// Resume the tracee (CONT/SYSCALL/SINGLESTEP/SYSEMU).
    /// Sets/clears the TRACE_* working bits, stores injected_signal, and wakes the tracee out of ptrace_stop.
    fn ptrace_resume_guarded(
        &self,
        token: PtraceFreezeToken,
        request: PtraceRequest,
        signal: Option<Signal>,
    ) -> Result<isize, SystemError> {
        // Validate the injected signal first
        let resume_signal = match signal {
            None => Signal::INVALID,
            Some(Signal::INVALID) => return Err(SystemError::EIO),
            Some(s) => s,
        };

        // Set/clear the syscall-trace / single-step working bits.
        match request {
            PtraceRequest::Singlestep => {
                // Re-verify under the lock that the tracee is still in a ptrace-stop before setting
                // RFLAGS.TF: a fatal signal may have already woken it, invalidating the stop frame.
                #[cfg(target_arch = "x86_64")]
                self.arm_trap_flag_single_step()?;
                self.flags().insert(ProcessFlags::TRACE_SINGLESTEP);
                self.flags()
                    .remove(ProcessFlags::TRACE_SYSCALL | ProcessFlags::TRACE_SYSEMU);
            }
            PtraceRequest::Syscall => {
                self.flags().insert(ProcessFlags::TRACE_SYSCALL);
                self.flags()
                    .remove(ProcessFlags::TRACE_SINGLESTEP | ProcessFlags::TRACE_SYSEMU);
                self.disable_single_step();
            }
            PtraceRequest::Sysemu | PtraceRequest::SysemuSinglestep => {
                self.flags().insert(ProcessFlags::TRACE_SYSEMU);
                if request == PtraceRequest::SysemuSinglestep {
                    self.flags().insert(ProcessFlags::TRACE_SINGLESTEP);
                    // SYSEMU_SINGLESTEP also needs hardware single-step armed (written after re-verification under the lock)
                    #[cfg(target_arch = "x86_64")]
                    self.arm_trap_flag_single_step()?;
                } else {
                    self.flags().remove(ProcessFlags::TRACE_SINGLESTEP);
                    self.disable_single_step();
                }
                self.flags().remove(ProcessFlags::TRACE_SYSCALL);
            }
            PtraceRequest::Cont => {
                self.flags().remove(
                    ProcessFlags::TRACE_SYSCALL
                        | ProcessFlags::TRACE_SINGLESTEP
                        | ProcessFlags::TRACE_SYSEMU,
                );
                self.disable_single_step();
            }
            _ => return Err(SystemError::EINVAL),
        }

        // Store the injected signal, clear the stop flags, and wake the tracee.
        let (was_in_stop, release) = {
            let mut ps = self.ptrace.state.lock_irqsave();
            if !ps.freeze_owner_matches(token) {
                return Err(SystemError::ESRCH);
            }
            ps.prepare_resume(resume_signal)?;
            let was_in_stop = self.sched_info().state().is_stopped();
            let release = ps.release_freeze_owner(token);
            (was_in_stop, release)
        };

        if was_in_stop && !release.fatal_wake {
            if let Some(strong) = self.self_ref.upgrade() {
                // Wake from Stopped back to Runnable
                let _ = ProcessManager::wakeup_stop(&strong);
            }
        }
        if let Some(strong) = self.self_ref.upgrade() {
            release.apply(&strong);
        }
        Ok(0)
    }
    /// ptrace-stop at syscall entry/exit (hot path).
    /// Returns true to indicate the syscall execution should be skipped (only meaningful for SYSEMU entry stops); false in all other cases.
    #[inline]
    pub fn ptrace_report_syscall(&self, is_entry: bool, nr: u64, args: &[usize; 6]) -> bool {
        let f = self.flags().load();
        let traced = if is_entry {
            f.contains(ProcessFlags::TRACE_SYSCALL) || f.contains(ProcessFlags::TRACE_SYSEMU)
        } else {
            f.contains(ProcessFlags::TRACE_SYSCALL) || f.contains(ProcessFlags::TRACE_SINGLESTEP)
        };
        if !traced {
            return false;
        }
        self.ptrace_report_syscall_slow(is_entry, nr, args, f)
    }

    /// Slow path of ptrace_report_syscall: the tracee has syscall-trace / single-step enabled,
    /// so a ptrace-stop must be constructed and synchronized with the tracer. Only called when the hot-path early return misses.
    #[cold]
    #[inline(never)]
    fn ptrace_report_syscall_slow(
        &self,
        is_entry: bool,
        _nr: u64,
        _args: &[usize; 6],
        flags: ProcessFlags,
    ) -> bool {
        let is_single_step = flags.contains(ProcessFlags::TRACE_SINGLESTEP);
        let msg = if is_entry {
            PTRACE_EVENTMSG_SYSCALL_ENTRY
        } else {
            PTRACE_EVENTMSG_SYSCALL_EXIT
        };
        let sysgood = self
            .ptrace
            .state
            .lock_irqsave()
            .options
            .contains(PtraceOptions::TRACESYSGOOD);
        let sysemu_skip = is_entry && flags.contains(ProcessFlags::TRACE_SYSEMU);
        // Single-stepping across the syscall exit: report a single-step trap
        if !is_entry && is_single_step {
            if let Ok(signr) = Self::ptrace_notify(Signal::SIGTRAP as usize, TRAP_TRACE) {
                Self::reinject_ptrace_signal(signr);
            }
        } else {
            // Pure syscall-stop (entry or non-single-step exit): the sysgood bit is only added to a pure syscall-stop.
            let exit_code = if sysgood {
                Signal::SIGTRAP as usize | PTRACE_SYSGOOD_BIT
            } else {
                Signal::SIGTRAP as usize
            };
            if let Ok(signr) = Self::ptrace_notify_with_message(exit_code, exit_code as i32, msg) {
                Self::reinject_ptrace_signal(signr);
            }
        }
        sysemu_skip
    }
}
