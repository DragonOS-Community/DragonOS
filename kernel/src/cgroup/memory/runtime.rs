//! Lock-free caller boundary for scoped allocation recovery.
//! Call only after abandoning unpublished candidates and releasing MM, VMA,
//! cache, inode, fd, cgroup UPDATE and IRQ guards. A retry is not a charge:
//! the caller repeats its original transaction and resolves its owner again.

use alloc::sync::{Arc, Weak};
use system_error::SystemError;

use super::{ChargeFailure, ChargeFailureReason, MemoryAccount, MemoryEvent};
use crate::mm::allocator::page_frame::PageFrameCount;
use crate::{
    arch::ipc::signal::Signal,
    mm::{
        memory_notify::RECOVERY_WAITERS,
        oom::{memory_limit_out_of_memory, memory_out_of_memory, MemoryOomOutcome, OomContext},
        oom_reaper::ReapStatus,
        oom_wait::PreparedOomWait,
        page::PageReclaimer,
        VirtAddr,
    },
    process::{ProcessFlags, ProcessManager},
};

const MAX_RECLAIM_RETRIES: u8 = 16; // Linux mm/internal.h
const PAGE_ALLOC_COSTLY_ORDER: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemoryAllocPolicy {
    TryOnly,
    ReclaimThenFail,
    ReclaimThenOom,
    /// A stacked I/O caller may retain FS/MM resources. Never start I/O,
    /// unmap a user MM, or wait for an OOM victim from this recovery path.
    ReclaimNoIo,
}

/// Enforce a policy already committed by a memory.max writer. The caller has
/// released UPDATE/file locks and retains the exact old controller instance.
/// Signals/no victim stop enforcement; the writer does not undo its policy.
pub(crate) fn enforce_max(account: &Arc<MemoryAccount>) -> Result<(), SystemError> {
    let mut remaining = MAX_RECLAIM_RETRIES;
    let mut rechecks = MAX_RECLAIM_RETRIES;
    let mut registration = None;
    loop {
        let task = ProcessManager::current_pcb();
        if Signal::signal_pending_state(true, false, &task)
            || task.flags().contains(ProcessFlags::EXITING)
        {
            return Err(SystemError::EINTR);
        }
        let ctx = OomContext {
            trigger_pid: task.raw_pid(),
            trigger_tgid: task.raw_tgid(),
            fault_address: VirtAddr::new(0),
            fault_ip: 0,
            order: 0,
        };
        drop(task);
        if !account.is_online() {
            return Ok(());
        }
        let Some(max) = account.policy().max else {
            return Ok(());
        };
        let usage = account.current_pages();
        if usage <= max {
            return Ok(());
        }
        if remaining != 0 {
            let count = usize::try_from(usage - max).map_err(|_| SystemError::ENOMEM)?;
            if PageReclaimer::shrink_account(account, PageFrameCount::new(count))?.reclaimed == 0 {
                remaining -= 1;
            }
            crate::sched::cond_resched();
            continue;
        }
        if registration.is_none() {
            registration = Some(PreparedOomWait::new()?.register(&RECOVERY_WAITERS));
        }
        match memory_limit_out_of_memory(account, ctx)? {
            MemoryOomOutcome::CurrentTaskKilled => return Err(SystemError::EINTR),
            MemoryOomOutcome::NoVictim => return Ok(()),
            MemoryOomOutcome::Recheck => {
                if rechecks == 0 {
                    return Ok(());
                }
                rechecks -= 1;
                crate::sched::sched_yield();
            }
            MemoryOomOutcome::Victim(victim) => {
                let epoch = account.release_epoch();
                loop {
                    let task = ProcessManager::current_pcb();
                    if Signal::signal_pending_state(true, false, &task) {
                        return Err(SystemError::EINTR);
                    }
                    drop(task);
                    if !account.is_online()
                        || account
                            .policy()
                            .max
                            .is_none_or(|max| account.current_pages() <= max)
                        || account.release_epoch() != epoch
                    {
                        break;
                    }
                    let done = victim.mm.upgrade().is_none_or(|mm| {
                        mm.id() != victim.mm_id
                            || mm.is_torn_down()
                            || matches!(mm.oom_reap_slot().status(), ReapStatus::Finished(_))
                    });
                    if done {
                        break;
                    }
                    registration
                        .as_ref()
                        .expect("writer waiter prepared")
                        .wait_interruptible()?;
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryDecision {
    RetryOperation,
    Exhausted,
}

/// Lives across retries of one outer transaction, not across unrelated syscalls.
pub(crate) struct ChargeRecoveryState {
    scope: Option<Weak<MemoryAccount>>,
    last_usage: u64,
    remaining: u8,
    rechecks: u8,
    offline_retries: u8,
    max_event_recorded: bool,
    noio_retries: u8,
}

impl ChargeRecoveryState {
    pub(crate) const fn new() -> Self {
        Self {
            scope: None,
            last_usage: 0,
            remaining: MAX_RECLAIM_RETRIES,
            rechecks: MAX_RECLAIM_RETRIES,
            offline_retries: MAX_RECLAIM_RETRIES,
            max_event_recorded: false,
            noio_retries: MAX_RECLAIM_RETRIES,
        }
    }
}

impl Default for ChargeRecoveryState {
    fn default() -> Self {
        Self::new()
    }
}

fn interrupted() -> bool {
    let task = ProcessManager::current_pcb();
    Signal::oom_fatal_signal_pending(&task) || task.flags().contains(ProcessFlags::EXITING)
}

fn has_margin(failure: &ChargeFailure) -> bool {
    let account = &failure.account;
    !account.is_online()
        || account.policy().max.is_none_or(|max| {
            account
                .current_pages()
                .checked_add(failure.requested_pages)
                .is_some_and(|usage| usage <= max)
        })
}

pub(crate) fn recover_charge(
    failure: &ChargeFailure,
    policy: MemoryAllocPolicy,
    order: usize,
    state: &mut ChargeRecoveryState,
) -> Result<RecoveryDecision, SystemError> {
    // Root/bootstrap can hit only the hard counter cap, before a current PCB
    // exists. This is not a reclaimable quota and needs no task lookup.
    if failure.reason == ChargeFailureReason::CounterOverflow {
        return Ok(RecoveryDecision::Exhausted);
    }
    if interrupted() {
        return Err(SystemError::EINTR);
    }
    if policy == MemoryAllocPolicy::TryOnly {
        return Ok(RecoveryDecision::Exhausted);
    }
    // Count every NOIO retry, including a concurrently restored margin or
    // offline routing. Those branches must not retain the upper I/O pin and
    // its resource ancestry indefinitely without ever entering shrink.
    if policy == MemoryAllocPolicy::ReclaimNoIo {
        if state.noio_retries == 0 {
            return Ok(RecoveryDecision::Exhausted);
        }
        state.noio_retries -= 1;
    }
    if failure.reason == ChargeFailureReason::Offline || !failure.account.is_online() {
        if state.offline_retries == 0 {
            return Ok(RecoveryDecision::Exhausted);
        }
        state.offline_retries -= 1;
        return Ok(RecoveryDecision::RetryOperation);
    }
    let account = &failure.account;
    let same_scope = state
        .scope
        .as_ref()
        .is_some_and(|scope| scope.ptr_eq(&Arc::downgrade(account)));
    let usage = account.current_pages();
    if same_scope && usage < state.last_usage && policy != MemoryAllocPolicy::ReclaimNoIo {
        // Actual usage decreased, not merely a reservation-cancellation epoch.
        state.remaining = MAX_RECLAIM_RETRIES;
        state.rechecks = MAX_RECLAIM_RETRIES;
    }
    if !same_scope {
        state.scope = Some(Arc::downgrade(account));
        state.max_event_recorded = false;
        // Do not renew the outer transaction's budget on ancestor oscillation.
    }
    state.last_usage = usage;
    if has_margin(failure) {
        return Ok(RecoveryDecision::RetryOperation);
    }
    if !state.max_event_recorded {
        account.record_event(MemoryEvent::Max);
        state.max_event_recorded = true;
    }
    while state.remaining != 0 {
        if interrupted() {
            return Err(SystemError::EINTR);
        }
        state.remaining -= 1;
        let count = usize::try_from(failure.requested_pages).map_err(|_| SystemError::ENOMEM)?;
        let reclaimed = if policy == MemoryAllocPolicy::ReclaimNoIo {
            PageReclaimer::shrink_account_noio(account, PageFrameCount::new(count))?
        } else {
            PageReclaimer::shrink_account(account, PageFrameCount::new(count))?
        };
        if has_margin(failure) {
            return Ok(RecoveryDecision::RetryOperation);
        }
        if reclaimed.reclaimed != 0 {
            if policy != MemoryAllocPolicy::ReclaimNoIo {
                state.remaining = MAX_RECLAIM_RETRIES;
            }
            state.last_usage = account.current_pages();
            return Ok(RecoveryDecision::RetryOperation);
        }
        crate::sched::cond_resched();
    }
    if policy != MemoryAllocPolicy::ReclaimThenOom || order > PAGE_ALLOC_COSTLY_ORDER {
        return Ok(RecoveryDecision::Exhausted);
    }
    // Allocate all wait metadata before selecting or killing anybody. Register
    // before selection so the kill->exit/reap->wake race cannot lose progress.
    let registration = PreparedOomWait::new()?.register(&RECOVERY_WAITERS);
    loop {
        if interrupted() {
            return Err(SystemError::EINTR);
        }
        if has_margin(failure) {
            return Ok(RecoveryDecision::RetryOperation);
        }
        let task = ProcessManager::current_pcb();
        let ctx = OomContext {
            trigger_pid: task.raw_pid(),
            trigger_tgid: task.raw_tgid(),
            fault_address: VirtAddr::new(0),
            fault_ip: 0,
            order,
        };
        drop(task);
        match memory_out_of_memory(failure, ctx)? {
            MemoryOomOutcome::CurrentTaskKilled => return Err(SystemError::EINTR),
            MemoryOomOutcome::NoVictim => return Ok(RecoveryDecision::Exhausted),
            MemoryOomOutcome::Recheck => {
                if state.rechecks == 0 {
                    return Ok(RecoveryDecision::Exhausted);
                }
                state.rechecks -= 1;
                // No committed victim identity: never register an indefinite
                // victim wait for selector contention or a stale candidate.
                crate::sched::sched_yield();
            }
            MemoryOomOutcome::Victim(victim) => {
                let epoch = account.release_epoch();
                loop {
                    if interrupted() {
                        return Err(SystemError::EINTR);
                    }
                    if has_margin(failure) || account.release_epoch() != epoch {
                        return Ok(RecoveryDecision::RetryOperation);
                    }
                    let done = victim.mm.upgrade().is_none_or(|mm| {
                        mm.id() != victim.mm_id
                            || mm.is_torn_down()
                            || matches!(mm.oom_reap_slot().status(), ReapStatus::Finished(_))
                    });
                    // The temporary MM pin above is gone before sleeping. A
                    // reaper job's own MM pin is not a final-Drop predicate.
                    if done {
                        return Ok(RecoveryDecision::RetryOperation);
                    }
                    registration.wait_killable()?;
                }
            }
        }
    }
}
