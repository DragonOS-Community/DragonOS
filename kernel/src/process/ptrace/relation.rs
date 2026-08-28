use super::{PtraceEvent, PtraceOptions};
use crate::{
    arch::ipc::signal::Signal,
    libs::rwlock::RwLock,
    process::{
        abi::WaitOption, cred, ExitState, ProcessControlBlock, ProcessFlags, RawPid,
        PTRACE_RELATION_LOCK,
    },
};
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{fence, AtomicU64, AtomicUsize, Ordering};
use system_error::SystemError;

/// Bidirectional ptrace ownership kept in every PCB.
///
/// `PTRACE_RELATION_LOCK` remains the only transaction lock. The inner lock
/// and atomic types are the pre-existing storage primitives, retained so this
/// ownership aggregation does not change lock count or hot-path semantics.
#[derive(Debug)]
pub(super) struct PtraceRelations {
    /// Tasks traced by this PCB when it acts as a tracer.
    tracees: RwLock<Vec<Arc<ProcessControlBlock>>>,
    /// This PCB's slot in its tracer's `tracees` vector.
    tracee_slot: AtomicUsize,
    /// The tracer of this PCB when it acts as a tracee.
    tracer: RwLock<Weak<ProcessControlBlock>>,
    /// Monotonic identity of the active tracing session.
    session_generation: AtomicU64,
}

impl PtraceRelations {
    pub(super) fn new() -> Self {
        Self {
            tracees: RwLock::new(Vec::new()),
            tracee_slot: AtomicUsize::new(NO_PTRACE_SLOT),
            tracer: RwLock::new(Weak::new()),
            session_generation: AtomicU64::new(0),
        }
    }

    #[inline]
    fn session_generation(&self) -> u64 {
        self.session_generation.load(Ordering::Acquire)
    }

    /// Advance ownership while the caller holds `PTRACE_RELATION_LOCK`.
    fn advance_session_generation_locked(&self) -> u64 {
        let mut next = self
            .session_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        if next == 0 {
            // Reserve zero as the initial unowned generation.
            self.session_generation.store(1, Ordering::Release);
            next = 1;
        }
        next
    }
}

const NO_PTRACE_SLOT: usize = usize::MAX;

/// Install both sides of a ptrace relation.  The caller must hold
/// `PTRACE_RELATION_LOCK`.
pub(super) fn link_relation_locked(
    tracee: &Arc<ProcessControlBlock>,
    tracer: &Arc<ProcessControlBlock>,
    options: PtraceOptions,
    seized: bool,
    initial_stop: Option<Signal>,
) -> Result<(), SystemError> {
    if tracee
        .ptrace
        .relations
        .tracer
        .read_irqsave()
        .upgrade()
        .is_some()
    {
        return Err(SystemError::EPERM);
    }

    let slot = {
        let mut tracees = tracer.ptrace.relations.tracees.write_irqsave();
        assert!(
            tracees.len() < tracees.capacity(),
            "ptrace relation link entered irqsave lock without reserved capacity"
        );
        let slot = tracees.len();
        tracees.push(tracee.clone());
        slot
    };
    tracee
        .ptrace
        .relations
        .tracee_slot
        .store(slot, Ordering::Relaxed);
    let generation = tracee.ptrace.relations.advance_session_generation_locked();
    *tracee.ptrace.relations.tracer.write_irqsave() = Arc::downgrade(tracer);

    // Publish the complete session configuration before PTRACED becomes
    // visible to lockless fast paths. Relation-bound readers are excluded by
    // PTRACE_RELATION_LOCK throughout this transaction.
    let mut state = tracee.ptrace.state.lock_irqsave();
    state.options = options;
    if seized {
        tracee.flags().insert(ProcessFlags::PT_SEIZED);
    } else {
        tracee.flags().remove(ProcessFlags::PT_SEIZED);
    }
    if let Some(signal) = initial_stop {
        state.queue_pending_stop(signal, generation, None);
        tracee.flags().insert(ProcessFlags::PENDING_PTRACE_STOP);
    }
    drop(state);
    tracee.flags().insert(ProcessFlags::PTRACED);
    Ok(())
}

/// Reserve admission capacity without holding the global irqsave relation
/// lock. A concurrent linker may consume it, so every caller must recheck
/// `len < capacity` after reacquiring `PTRACE_RELATION_LOCK` and retry.
pub(super) fn reserve_relation_slot(tracer: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
    tracer
        .ptrace
        .relations
        .tracees
        .write()
        .try_reserve(1)
        .map_err(|_| SystemError::ENOMEM)
}

pub(super) fn relation_slot_available_locked(tracer: &Arc<ProcessControlBlock>) -> bool {
    let tracees = tracer.ptrace.relations.tracees.read_irqsave();
    tracees.len() < tracees.capacity()
}

/// Remove both sides of a ptrace relation in O(1).  The caller must hold
/// `PTRACE_RELATION_LOCK`.
pub(super) fn unlink_relation_locked(
    tracee: &Arc<ProcessControlBlock>,
) -> Option<Arc<ProcessControlBlock>> {
    let tracer = tracee.ptrace.relations.tracer.read_irqsave().upgrade()?;
    let slot = tracee.ptrace.relations.tracee_slot.load(Ordering::Relaxed);
    let moved = {
        let mut tracees = tracer.ptrace.relations.tracees.write_irqsave();
        assert!(slot < tracees.len(), "ptrace relation slot out of bounds");
        assert!(
            Arc::ptr_eq(&tracees[slot], tracee),
            "ptrace relation slot points at another tracee"
        );
        tracees.swap_remove(slot);
        tracees.get(slot).cloned()
    };
    if let Some(moved) = moved {
        moved
            .ptrace
            .relations
            .tracee_slot
            .store(slot, Ordering::Relaxed);
    }

    tracee.ptrace.relations.advance_session_generation_locked();
    tracee
        .ptrace
        .relations
        .tracee_slot
        .store(NO_PTRACE_SLOT, Ordering::Relaxed);
    *tracee.ptrace.relations.tracer.write_irqsave() = Weak::new();
    tracee.flags().remove(ProcessFlags::PTRACED);
    Some(tracer)
}

/// Pop one relation owned by `tracer` without allocating.  The caller must
/// hold `PTRACE_RELATION_LOCK`.
pub(super) fn pop_tracee_locked(
    tracer: &Arc<ProcessControlBlock>,
) -> Option<Arc<ProcessControlBlock>> {
    let tracee = tracer.ptrace.relations.tracees.write_irqsave().pop()?;
    let expected_slot = tracer.ptrace.relations.tracees.read_irqsave().len();
    assert_eq!(
        tracee.ptrace.relations.tracee_slot.load(Ordering::Relaxed),
        expected_slot,
        "popped ptrace relation has a stale slot"
    );
    assert!(
        tracee
            .ptrace
            .relations
            .tracer
            .read_irqsave()
            .upgrade()
            .map(|owner| Arc::ptr_eq(&owner, tracer))
            .unwrap_or(false),
        "popped tracee belongs to another tracer"
    );
    tracee.ptrace.relations.advance_session_generation_locked();
    tracee
        .ptrace
        .relations
        .tracee_slot
        .store(NO_PTRACE_SLOT, Ordering::Relaxed);
    *tracee.ptrace.relations.tracer.write_irqsave() = Weak::new();
    tracee.flags().remove(ProcessFlags::PTRACED);
    Some(tracee)
}

pub fn tracees_of(tracer: &Arc<ProcessControlBlock>) -> Vec<RawPid> {
    // This is a weak wait-candidate snapshot; report consumption revalidates
    // the relation transactionally.  Relation writers already serialize the
    // vector through its own RwLock, and no hardirq path mutates the index, so
    // do not keep the global IRQ-off transaction across an O(n) copy.
    let initial_len = tracer.ptrace.relations.tracees.read().len();
    let mut result = Vec::with_capacity(initial_len);

    loop {
        let required = {
            let tracees = tracer.ptrace.relations.tracees.read();
            if result.capacity() >= tracees.len() {
                result.clear();
                result.extend(tracees.iter().map(|tracee| tracee.raw_pid()));
                return result;
            }
            tracees.len()
        };

        // `reserve` is expressed relative to len (which is zero until the
        // successful snapshot), not relative to the current capacity.
        result.reserve(required);
    }
}

pub fn ptracer_of(tracee: &Arc<ProcessControlBlock>) -> Option<Arc<ProcessControlBlock>> {
    // Fast path: the PTRACED bit is only written together with ptracer inside the relation-lock
    // critical section; if the bit is clear there is necessarily no tracer, so the global lock is unnecessary.
    if !tracee.flags().contains(ProcessFlags::PTRACED) {
        return None;
    }
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    ptracer_of_locked(tracee)
}

impl ProcessControlBlock {
    /// Snapshot the ownership generation without taking the relation lock.
    /// Deferred debug records use the Acquire load to reject stale sessions.
    pub(crate) fn ptrace_session_generation(&self) -> u64 {
        self.ptrace.relations.session_generation()
    }
}

fn fork_event_option(event: PtraceEvent) -> Option<PtraceOptions> {
    match event {
        PtraceEvent::Fork => Some(PtraceOptions::TRACEFORK),
        PtraceEvent::VFork => Some(PtraceOptions::TRACEVFORK),
        PtraceEvent::Clone => Some(PtraceOptions::TRACECLONE),
        _ => None,
    }
}

/// Immutable identity and inherited state of the source tracing session for
/// one fork transaction.
pub(crate) struct PtraceForkSession {
    tracer: Arc<ProcessControlBlock>,
    source_generation: u64,
    seized: bool,
    options: PtraceOptions,
    event: PtraceEvent,
    report_event: bool,
}

impl PtraceForkSession {
    pub(crate) fn tracer(&self) -> &Arc<ProcessControlBlock> {
        &self.tracer
    }

    pub(crate) fn source_generation(&self) -> u64 {
        self.source_generation
    }

    pub(crate) fn event(&self) -> PtraceEvent {
        self.event
    }

    pub(crate) fn report_event(&self) -> bool {
        self.report_event
    }
}

/// Snapshot one fork-inheritance candidate while the caller holds
/// `PTRACE_RELATION_LOCK`. The boolean reports whether the tracer already has
/// admission capacity for the child relation; allocation must be performed
/// only after dropping the irqsave transaction locks.
pub(crate) fn ptrace_fork_session_snapshot_locked(
    source: &Arc<ProcessControlBlock>,
    event: PtraceEvent,
    explicit_clone_ptrace: bool,
) -> Option<(PtraceForkSession, bool)> {
    let event_option = fork_event_option(event)?;
    let tracer = ptracer_of_locked(source)?;
    let state = source.ptrace.state.lock_irqsave();
    let options = state.options;
    let report_event = options.contains(event_option);
    if !explicit_clone_ptrace && !report_event {
        return None;
    }
    let session = PtraceForkSession {
        tracer,
        source_generation: source.ptrace_session_generation(),
        seized: source.flags().contains(ProcessFlags::PT_SEIZED),
        options,
        event,
        report_event,
    };
    let slot_available = relation_slot_available_locked(&session.tracer);
    Some((session, slot_available))
}

pub(crate) fn ptracer_of_locked(
    tracee: &Arc<ProcessControlBlock>,
) -> Option<Arc<ProcessControlBlock>> {
    tracee.ptrace.relations.tracer.read_irqsave().upgrade()
}

pub fn is_ptraced(tracee: &ProcessControlBlock) -> bool {
    // Fast path like ptracer_of: avoid the global lock when not traced.
    if !tracee.flags().contains(ProcessFlags::PTRACED) {
        return false;
    }
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    is_ptraced_locked(tracee)
}

pub(super) fn is_ptraced_locked(tracee: &ProcessControlBlock) -> bool {
    tracee.flags().contains(ProcessFlags::PTRACED)
        && tracee
            .ptrace
            .relations
            .tracer
            .read_irqsave()
            .upgrade()
            .is_some()
}

/// Atomically validate that a deferred ptrace-owned event still belongs to
/// the currently installed tracing relation. A detach followed by reattach
/// must not hand an old event to the new tracer.
pub(crate) fn ptrace_session_matches(tracee: &ProcessControlBlock, generation: u64) -> bool {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    tracee.ptrace_session_generation() == generation && is_ptraced_locked(tracee)
}

/// Snapshot debug-event ownership under the relation lock.  The CPU debug
/// shadow may predate a running PTRACE_SEIZE, so an active relation always
/// owns a subsequent hardware event at the relation's current generation.
pub(crate) fn ptrace_debug_session_snapshot(tracee: &ProcessControlBlock) -> (Option<u64>, u64) {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    let generation = tracee.ptrace_session_generation();
    (is_ptraced_locked(tracee).then_some(generation), generation)
}

pub fn is_wait_tracee_of(
    tracee: &Arc<ProcessControlBlock>,
    waiter: &Arc<ProcessControlBlock>,
    options: WaitOption,
) -> bool {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    let Some(tracer) = ptracer_of_locked(tracee) else {
        return false;
    };

    let same_waiter = Arc::ptr_eq(&tracer, waiter);
    let same_thread_group = !options.contains(WaitOption::WNOTHREAD) && tracer.tgid == waiter.tgid;
    if !same_waiter && !same_thread_group {
        return false;
    }

    // Both directions are committed under the relation lock; once the
    // ptracer matches, a second O(N) tracer-index scan is redundant.
    true
}

/// Validate ptrace wait ownership and consume the reportable stop in one
/// relation transaction.  A detach/reattach cannot otherwise be allowed to
/// turn an eligibility decision for the old tracer into consumption of the
/// new tracing session's stop record.
pub(crate) fn consume_wait_ptrace_stop_report(
    tracee: &Arc<ProcessControlBlock>,
    waiter: &Arc<ProcessControlBlock>,
    options: WaitOption,
    consume: bool,
) -> Option<i32> {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    let tracer = ptracer_of_locked(tracee)?;
    let same_waiter = Arc::ptr_eq(&tracer, waiter);
    let same_thread_group = !options.contains(WaitOption::WNOTHREAD) && tracer.tgid == waiter.tgid;
    if !same_waiter && !same_thread_group {
        return None;
    }
    if !tracee.sched_info().state().is_stopped() {
        return None;
    }
    tracee
        .ptrace
        .state
        .lock_irqsave()
        .consume_stop_report(consume)
}

/// Source of the caller's credentials used for access checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PtraceAccessCreds {
    /// Filesystem paths such as procfs: judged by fsuid/fsgid and the effective capability set
    FsCreds,
    /// Explicit syscalls: judged by the real uid/gid and the permitted capability set
    RealCreds,
}

// PCB ptrace methods -- relation establishment/teardown, attach/seize/detach, TRAPPING synchronization
impl ProcessControlBlock {
    pub fn has_permission_to_trace(&self, tracee: &Self, creds: PtraceAccessCreds) -> bool {
        // 1. The same thread group is always allowed access (introspection)
        if self.tgid == tracee.tgid {
            return true;
        }

        let caller_cred = self.cred();
        let tracee_cred = tracee.cred();
        let same_user_ns = Arc::ptr_eq(&caller_cred.user_ns, &tracee_cred.user_ns);
        let tracee_mm = tracee.basic().user_vm();
        // The caller's identity is selected according to the credential mode.
        let (caller_uid, caller_gid) = match creds {
            PtraceAccessCreds::FsCreds => (caller_cred.fsuid, caller_cred.fsgid),
            PtraceAccessCreds::RealCreds => (caller_cred.uid, caller_cred.gid),
        };
        // 2. Credential match + dumpable
        let uid_match = caller_uid == tracee_cred.euid
            && caller_uid == tracee_cred.suid
            && caller_uid == tracee_cred.uid;
        let gid_match = caller_gid == tracee_cred.egid
            && caller_gid == tracee_cred.sgid
            && caller_gid == tracee_cred.gid;
        // 3. CAP_SYS_PTRACE: the capability is evaluated in the target (tracee)'s user_ns,
        // not the caller's own ns, preventing a child user namespace from tracing a parent-ns process beyond its authority.
        let has_cap_in_task_ns = || {
            caller_cred.has_capability_in_ns(&tracee_cred.user_ns, cred::CAPFlags::CAP_SYS_PTRACE)
        };

        // Read-side barrier: pairs with the write-side barrier on the credential-commit path -- the write side
        // publishes dumpability first, then the new credentials; the read side inserts a barrier after reading the
        // tracee's credentials and before reading dumpable, so it never observes the "new credentials + old dumpable" window (attach at a privilege drop).
        fence(Ordering::SeqCst);

        if !(has_cap_in_task_ns() || same_user_ns && uid_match && gid_match) {
            return false;
        }

        let dumpable = tracee_mm
            .as_ref()
            .map(|mm| mm.dumpable())
            .unwrap_or(cred::SUID_DUMP_DISABLE as u8);
        if dumpable != cred::SUID_DUMP_USER as u8 {
            let mm_user_ns = tracee_mm
                .as_ref()
                .map(|mm| mm.user_ns())
                .unwrap_or_else(|| {
                    crate::process::namespace::user_namespace::INIT_USER_NAMESPACE.clone()
                });
            if !caller_cred.has_capability_in_ns(&mm_user_ns, cred::CAPFlags::CAP_SYS_PTRACE) {
                return false;
            }
        }

        // 4. Capability subset gate: the target's permitted set must be a subset of the caller's capability set (same user_ns).
        let caller_caps = match creds {
            PtraceAccessCreds::FsCreds => caller_cred.cap_effective,
            PtraceAccessCreds::RealCreds => caller_cred.cap_permitted,
        };
        (same_user_ns && (tracee_cred.cap_permitted.bits() & !caller_caps.bits()) == 0)
            || has_cap_in_task_ns()
    }

    /// Establish a tracing relation (called on the tracee side).
    /// The caller need not hold `PTRACE_RELATION_LOCK`; the function acquires it itself.
    pub fn ptrace_link(&self, tracer: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        if !tracer.has_permission_to_trace(self, PtraceAccessCreds::RealCreds) {
            return Err(SystemError::EPERM);
        }
        // ATTACH publishes its empty option set together with the relation.
        self.ptrace_link_configured(tracer, PtraceOptions::empty(), false)
    }

    /// Reserve one possible inherited-relation slot outside the irqsave
    /// publication transaction. A concurrent linker may consume the capacity,
    /// so the caller must retry its relation-locked snapshot afterwards.
    pub(crate) fn ptrace_reserve_fork_relation_slot(
        session: &PtraceForkSession,
    ) -> Result<(), SystemError> {
        reserve_relation_slot(&session.tracer)
    }

    /// Commit a fork child's inherited relation while the caller holds
    /// `PTRACE_RELATION_LOCK`, after checking admission capacity in the same
    /// transaction. A stale source session simply means that this child does
    /// not inherit tracing.
    pub(crate) fn ptrace_link_inherit_from_locked(
        &self,
        source: &Arc<ProcessControlBlock>,
        session: &PtraceForkSession,
    ) -> bool {
        if source.ptrace_session_generation() != session.source_generation
            || !ptracer_of_locked(source)
                .map(|owner| Arc::ptr_eq(&owner, &session.tracer))
                .unwrap_or(false)
        {
            return false;
        }

        let source_state = source.ptrace.state.lock_irqsave();
        if source_state.options != session.options
            || source.flags().contains(ProcessFlags::PT_SEIZED) != session.seized
            || session.tracer.exit_state() != ExitState::Running
            || session.tracer.flags().contains(ProcessFlags::EXITING)
        {
            return false;
        }
        drop(source_state);

        assert_eq!(
            self.exit_state(),
            ExitState::Running,
            "unpublished fork child cannot already be exiting"
        );
        assert!(
            self.ptrace
                .relations
                .tracer
                .read_irqsave()
                .upgrade()
                .is_none(),
            "unpublished fork child already has a ptracer"
        );
        assert!(
            relation_slot_available_locked(&session.tracer),
            "fork relation capacity was not reserved before publication"
        );
        let tracee = self
            .self_ref
            .upgrade()
            .expect("fork child lost its self reference before publication");
        link_relation_locked(
            &tracee,
            &session.tracer,
            session.options,
            session.seized,
            session.seized.then_some(Signal::SIGTRAP),
        )
        .expect("validated fork relation link failed");

        if !session.seized {
            // Linux ptrace_init_task() places SIGSTOP directly in the new
            // task's private pending bitmap. The child is still scheduler-New,
            // so no signal delivery, allocation, or wakeup is needed here.
            let sighand = self.sighand();
            let _sighand_guard = sighand.inner_read();
            self.sig_info_mut()
                .sig_pending_mut()
                .signal_mut()
                .insert(Signal::SIGSTOP.into());
            self.flags().insert(ProcessFlags::HAS_PENDING_SIGNAL);
        }
        true
    }

    pub(super) fn ptrace_link_configured(
        &self,
        tracer: &Arc<ProcessControlBlock>,
        options: PtraceOptions,
        seized: bool,
    ) -> Result<(), SystemError> {
        loop {
            {
                let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();

                if tracer.exit_state() != ExitState::Running
                    || tracer.flags().contains(ProcessFlags::EXITING)
                {
                    return Err(SystemError::EPERM);
                }

                // Refuse targets that are exiting or have already exited.
                if self.exit_state() != ExitState::Running {
                    return Err(SystemError::EPERM);
                }
                if self
                    .ptrace
                    .relations
                    .tracer
                    .read_irqsave()
                    .upgrade()
                    .is_some()
                {
                    return Err(SystemError::EPERM);
                }
                if relation_slot_available_locked(tracer) {
                    let tracee = self.self_ref.upgrade().ok_or(SystemError::ESRCH)?;
                    link_relation_locked(&tracee, tracer, options, seized, None)?;
                    return Ok(());
                }
            }

            reserve_relation_slot(tracer)?;
            // Capacity is only an admission reservation. Another concurrent
            // linker may consume it before this task reacquires relation lock.
        }
    }

    /// Whether this process is currently traced.
    pub fn is_traced(&self) -> bool {
        // Fast path: the PTRACED bit is only set/cleared inside the relation-lock critical section, so the global lock is unnecessary.
        if !self.flags().contains(ProcessFlags::PTRACED) {
            return false;
        }
        let _g = PTRACE_RELATION_LOCK.lock_irqsave();
        is_ptraced_locked(self)
    }

    /// Whether this process is currently traced by the given tracer.
    pub fn is_traced_by(&self, tracer: &Arc<ProcessControlBlock>) -> bool {
        match self.self_ref.upgrade() {
            Some(me) => match ptracer_of(&me) {
                Some(t) => Arc::ptr_eq(&t, tracer),
                None => false,
            },
            None => false,
        }
    }

    /// Ownership check for PTRACE_KILL/PTRACE_INTERRUPT, which Linux permits
    /// without freezing or requiring TASK_TRACED.
    pub(crate) fn ptrace_check_non_frozen(
        &self,
        tracer: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
        let tracee = self.self_ref.upgrade().ok_or(SystemError::ESRCH)?;
        if !ptracer_of_locked(&tracee)
            .map(|owner| Arc::ptr_eq(&owner, tracer))
            .unwrap_or(false)
        {
            return Err(SystemError::ESRCH);
        }
        Ok(())
    }
}
