use alloc::{
    format,
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, Ordering};
use log::{error, warn};
use system_error::SystemError;

use crate::{
    arch::{ipc::signal::Signal, mm::LockedFrameAllocator, MMArch},
    cgroup::memory::{
        account_for_task, ChargeFailure, ChargeFailureReason, MemoryAccount, MemoryEvent,
    },
    ipc::signal::PreparedOomSigkill,
    libs::{spinlock::SpinLock, wait_queue::WaitQueue},
    mm::{allocator::page_frame::FrameAllocator, MemoryManagementArch},
    process::{ProcessControlBlock, ProcessFlags, ProcessManager, RawPid},
};

use super::ucontext::AddressSpace;

/// Permission issued only after a committed kill and shared-init exclusion.
/// Keep its field private to this publisher; an MM id alone is not permission.
pub(crate) struct OomReapAuthorization {
    mm_id: u64,
}

impl OomReapAuthorization {
    pub(in crate::mm) fn mm_id(&self) -> u64 {
        self.mm_id
    }

    #[cfg(test)]
    pub(in crate::mm) fn test_authorization(mm_id: u64) -> Self {
        Self { mm_id }
    }
}

static OOM_WAITQ: WaitQueue = WaitQueue::default();
static OOM_STATE: SpinLock<OomState> = SpinLock::new(OomState::new());
static OOM_FAULT_INJECT: SpinLock<OomFaultInject> = SpinLock::new(OomFaultInject::disabled());
static OOM_KILL_COUNT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub struct OomContext {
    pub trigger_pid: RawPid,
    pub trigger_tgid: RawPid,
    pub fault_address: super::VirtAddr,
    pub fault_ip: usize,
    pub order: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OomOutcome {
    Retry,
    CurrentTaskKilled,
    NoVictim,
}

/// Identity of a real, committed OOM victim. Waiting uses mm teardown, not its
/// final Arc drop: keeping a strong reference here would delay that drop.
#[derive(Debug)]
pub(crate) struct MemoryOomVictim {
    pub(crate) mm: Weak<AddressSpace>,
    pub(crate) mm_id: u64,
}

#[derive(Debug)]
pub(crate) enum MemoryOomOutcome {
    Victim(MemoryOomVictim),
    Recheck,
    CurrentTaskKilled,
    NoVictim,
}

enum OomSelection {
    Candidate(OomCandidate),
    ExistingVictim(MemoryOomVictim),
    None,
}

struct CommittedOomVictim {
    tgid: RawPid,
    identity: MemoryOomVictim,
}

#[derive(Debug, Clone)]
struct OomVictimState {
    generation: u64,
    tgid: RawPid,
    mm_id: u64,
}

#[derive(Debug)]
struct OomState {
    generation: u64,
    selecting: bool,
    inflight: Option<OomVictimState>,
}

#[derive(Debug)]
struct OomFaultInject {
    target_tgid: Option<RawPid>,
    fail_after: usize,
    seen: usize,
    remaining_failures: Option<usize>,
}

impl OomState {
    const fn new() -> Self {
        Self {
            generation: 0,
            selecting: false,
            inflight: None,
        }
    }
}

impl OomFaultInject {
    const fn disabled() -> Self {
        Self {
            target_tgid: None,
            fail_after: 0,
            seen: 0,
            remaining_failures: Some(0),
        }
    }

    fn is_enabled(&self) -> bool {
        self.target_tgid.is_some()
    }
}

#[derive(Debug)]
struct OomCandidate {
    task: Arc<ProcessControlBlock>,
    tgid: RawPid,
    mm: Arc<AddressSpace>,
    score: isize,
    resident_pages: usize,
    oom_score_adj: i16,
}

struct OomKillTarget {
    tgid: RawPid,
    signal: PreparedOomSigkill,
    committed: bool,
    event_account: Option<Arc<MemoryAccount>>,
}

const OOM_SCORE_ADJ_MIN: i16 = -1000;

#[cfg(test)]
mod scoped_score_tests {
    use super::scoped_badness;

    #[test]
    fn small_domain_adjustment_uses_linux_division_order() {
        assert_eq!(scoped_badness(40, 2, 999, 999), 42);
        assert_eq!(scoped_badness(40, 2, -999, 999), 42);
        assert_eq!(scoped_badness(40, 2, 100, 1999), 142);
    }

    #[test]
    fn page_tables_are_real_score_and_large_values_saturate() {
        assert_eq!(scoped_badness(100, 4, 0, 1000), 104);
        assert_eq!(scoped_badness(usize::MAX, 4, 1000, isize::MAX), isize::MAX);
    }
}

fn wake_oom_waiters() {
    OOM_WAITQ.wake_all();
}

fn current_is_killed_or_exiting() -> bool {
    let current = ProcessManager::current_pcb();
    Signal::oom_fatal_signal_pending(&current) || current.flags().intersects(ProcessFlags::EXITING)
}

fn leader_of(pcb: Arc<ProcessControlBlock>) -> Arc<ProcessControlBlock> {
    ProcessManager::find(pcb.raw_tgid()).unwrap_or(pcb)
}

fn is_global_init_or_kthread(leader: &Arc<ProcessControlBlock>) -> bool {
    leader.raw_pid().data() == 0
        || leader.raw_tgid().data() == 1
        || leader.flags().contains(ProcessFlags::KTHREAD)
}

fn should_skip_candidate(task: &Arc<ProcessControlBlock>, oom_score_adj: i16) -> bool {
    is_global_init_or_kthread(&leader_of(task.clone()))
        || task.flags().contains(ProcessFlags::EXITING)
        || task.is_active_vfork()
        || oom_score_adj == OOM_SCORE_ADJ_MIN
}

fn better_candidate(new: &OomCandidate, old: &OomCandidate) -> bool {
    new.score > old.score
        || (new.score == old.score && new.resident_pages > old.resident_pages)
        || (new.score == old.score
            && new.resident_pages == old.resident_pages
            && new.tgid > old.tgid)
}

fn total_system_pages() -> isize {
    let total_pages = unsafe { LockedFrameAllocator.usage() }.total().bytes() >> MMArch::PAGE_SHIFT;
    total_pages.min(isize::MAX as usize).max(1) as isize
}

fn oom_score(mm: &Arc<AddressSpace>, oom_score_adj: i16, total_pages: isize) -> isize {
    let resident_pages = mm.resident_pages().min(isize::MAX as usize) as isize;
    let adjustment = (oom_score_adj as isize).saturating_mul(total_pages) / 1000;
    resident_pages.saturating_add(adjustment)
}

fn scoped_badness(rss: usize, page_tables: usize, adj: i16, total_pages: isize) -> isize {
    let pages = rss.saturating_add(page_tables).min(isize::MAX as usize) as isize;
    pages.saturating_add((adj as isize).saturating_mul(total_pages / 1000))
}

pub fn proc_oom_score(pcb: &Arc<ProcessControlBlock>) -> usize {
    let leader = leader_of(pcb.clone());
    if is_global_init_or_kthread(&leader) || leader.is_active_vfork() {
        return 0;
    }

    let oom_score_adj = pcb.sig_info_irqsave().oom_score_adj();
    if oom_score_adj == OOM_SCORE_ADJ_MIN {
        return 0;
    }

    let Some(mm) = pcb.basic().user_vm() else {
        return 0;
    };
    let total_pages = total_system_pages();
    let badness = oom_score(&mm, oom_score_adj, total_pages);
    let score = 1000isize
        .saturating_add(badness.saturating_mul(1000) / total_pages)
        .saturating_mul(2)
        / 3;
    score.clamp(0, 2000) as usize
}

pub fn oom_kill_count() -> u64 {
    OOM_KILL_COUNT.load(Ordering::Relaxed)
}

fn count_oom_kill() {
    OOM_KILL_COUNT.fetch_add(1, Ordering::Relaxed);
}

fn task_uses_mm(task: &Arc<ProcessControlBlock>, mm: &Arc<AddressSpace>) -> bool {
    task.basic()
        .user_vm()
        .is_some_and(|task_mm| task_mm.id() == mm.id() || Arc::ptr_eq(&task_mm, mm))
}

fn clear_inflight_for_mm(mm_id: u64) -> bool {
    let mut state = OOM_STATE.lock_irqsave();
    if state
        .inflight
        .as_ref()
        .is_some_and(|victim| victim.mm_id == mm_id)
    {
        state.inflight = None;
        true
    } else {
        false
    }
}

fn select_victim(scope: Option<&Arc<MemoryAccount>>) -> Result<OomSelection, SystemError> {
    let tasks = crate::process::snapshot_all_processes()?;
    let total_pages = scope.map_or_else(total_system_pages, |scope| {
        scope
            .policy()
            .max
            .unwrap_or(1)
            .max(1)
            .min(isize::MAX as u64) as isize
    });
    let mut seen_tgids = Vec::new();
    seen_tgids
        .try_reserve(tasks.len())
        .map_err(|_| SystemError::ENOMEM)?;
    let mut best: Option<OomCandidate> = None;

    for task in tasks {
        if scope.is_some_and(|scope| !account_for_task(&task).is_descendant_of(scope)) {
            continue;
        }
        let leader = leader_of(task.clone());
        let tgid = leader.raw_tgid();
        if seen_tgids.contains(&tgid) {
            continue;
        }
        if scope.is_some() {
            if is_global_init_or_kthread(&leader) {
                continue;
            }
            if let Some((mm_id, mm)) = leader.process_signal().recorded_oom_victim(tgid) {
                // Linux checks signal->oom_mm before badness. An exec may have
                // replaced the task's current mm; never wait on that new image.
                let unfinished = mm.upgrade().is_some_and(|mm| {
                    !mm.is_torn_down()
                        && !matches!(
                            mm.oom_reap_slot().status(),
                            super::oom_reaper::ReapStatus::Finished(_)
                        )
                });
                if unfinished {
                    return Ok(OomSelection::ExistingVictim(MemoryOomVictim { mm, mm_id }));
                }
                // A completed recorded victim is like MMF_OOM_SKIP. Do not
                // count or select it again, even if its adjustment changed.
                continue;
            }
        }
        let Some(mm) = task.basic().user_vm() else {
            continue;
        };
        if mm.is_torn_down()
            || matches!(
                mm.oom_reap_slot().status(),
                super::oom_reaper::ReapStatus::Finished(_)
            )
        {
            continue;
        }
        // A leader without an mm must not hide a remaining mm-holding worker.
        seen_tgids.push(tgid);
        let oom_score_adj = leader.sig_info_irqsave().oom_score_adj();
        if should_skip_candidate(&task, oom_score_adj) {
            continue;
        }

        let candidate = OomCandidate {
            task,
            tgid,
            score: if scope.is_some() {
                scoped_badness(
                    mm.resident_pages(),
                    mm.page_table_pages(),
                    oom_score_adj,
                    total_pages,
                )
            } else {
                oom_score(&mm, oom_score_adj, total_pages)
            },
            resident_pages: mm.resident_pages(),
            oom_score_adj,
            mm,
        };
        if best
            .as_ref()
            .is_none_or(|current| better_candidate(&candidate, current))
        {
            best = Some(candidate);
        }
    }

    Ok(best.map_or(OomSelection::None, OomSelection::Candidate))
}

fn begin_selection(scoped: bool) -> Result<u64, ()> {
    let mut state = OOM_STATE.lock_irqsave();
    if state.selecting || (!scoped && state.inflight.is_some()) {
        Err(())
    } else {
        state.selecting = true;
        state.generation = state.generation.wrapping_add(1);
        Ok(state.generation)
    }
}

fn finish_selection_none() {
    {
        let mut state = OOM_STATE.lock_irqsave();
        state.selecting = false;
    }
    wake_oom_waiters();
    crate::mm::memory_notify::request_oom_progress();
}

pub fn note_oom_victim_mm_released(mm_id: u64) {
    if clear_inflight_for_mm(mm_id) {
        wake_oom_waiters();
    }
    crate::mm::memory_notify::request_oom_progress();
}

/// Like MMF_OOM_SKIP: another selection can proceed even if references keep
/// the scanned MM alive. This is not a physical-frame release notification.
pub(crate) fn note_oom_reaping_finished(mm_id: u64) {
    if clear_inflight_for_mm(mm_id) {
        wake_oom_waiters();
    }
    crate::mm::memory_notify::request_oom_progress();
}

fn send_oom_sigkill(
    generation: u64,
    candidate: &OomCandidate,
    scope: Option<&Arc<MemoryAccount>>,
    requested_pages: u64,
) -> Result<CommittedOomVictim, SystemError> {
    let mut preparation_retries = 16;
    loop {
        let tasks = crate::process::snapshot_all_processes()?;
        let victim = candidate.task.clone();
        if victim.raw_tgid() != candidate.tgid || !task_uses_mm(&victim, &candidate.mm) {
            return Err(SystemError::ESRCH);
        }
        let victim_tgid = victim.raw_tgid();
        let victim_account = account_for_task(&victim);
        let mut group = None;
        let mut account = victim_account.clone();
        if scope.is_none_or(|scope| account.is_descendant_of(scope)) {
            loop {
                if account.policy().oom_group {
                    group = Some(account.clone());
                }
                if scope.is_some_and(|scope| Arc::ptr_eq(scope, &account)) {
                    break;
                }
                match account.parent() {
                    Some(parent) => account = parent.clone(),
                    None => break,
                }
            }
        }
        // All process-group signal storage is prepared before the first kill.
        // This also permits cleanup of every group victim's real mm, without
        // authorizing cleanup using an uncommitted, stale snapshot source.
        let mut primary = PreparedOomSigkill::prepare(victim.clone())?;
        let mut secondary: Vec<OomKillTarget> = Vec::new();
        secondary
            .try_reserve(tasks.len())
            .map_err(|_| SystemError::ENOMEM)?;
        let mut authorization_tasks = Vec::new();
        authorization_tasks
            .try_reserve(tasks.len())
            .map_err(|_| SystemError::ENOMEM)?;
        let mut prospective_mms = Vec::new();
        prospective_mms
            .try_reserve(tasks.len().saturating_add(1))
            .map_err(|_| SystemError::ENOMEM)?;
        let mut killed_mms = Vec::new();
        killed_mms
            .try_reserve(tasks.len().saturating_add(1))
            .map_err(|_| SystemError::ENOMEM)?;
        let mut retired_mms = Vec::new();
        retired_mms
            .try_reserve(tasks.len().saturating_add(1))
            .map_err(|_| SystemError::ENOMEM)?;
        for task in &tasks {
            let task = task.clone();
            let leader = leader_of(task.clone());
            if leader.raw_tgid() == victim_tgid
                || is_global_init_or_kthread(&leader)
                || task.basic().user_vm().is_none()
                || secondary.iter().any(|kill| kill.tgid == task.raw_tgid())
            {
                continue;
            }
            secondary.push(OomKillTarget {
                tgid: task.raw_tgid(),
                signal: PreparedOomSigkill::prepare(task)?,
                committed: false,
                event_account: None,
            });
        }
        primary.clear_snapshot();
        for kill in &mut secondary {
            kill.signal.clear_snapshot();
        }
        // Mutex before IRQ locks; all wakeups and batch reference retirement
        // happen after both publication and adjustment locks are released.
        let adj_guard = ProcessManager::lock_oom_score_adj();
        let membership = crate::process::pid::pid_membership_lock();
        if crate::process::snapshot_all_processes_into(&mut authorization_tasks).is_err()
            || authorization_tasks
                .len()
                .checked_add(1)
                .is_none_or(|required| required > prospective_mms.capacity())
            || primary.refresh_snapshot_locked().is_err()
            || secondary
                .iter_mut()
                .any(|kill| kill.signal.refresh_snapshot_locked().is_err())
        {
            drop(membership);
            drop(adj_guard);
            if preparation_retries == 0 {
                return Err(SystemError::ESRCH);
            }
            preparation_retries -= 1;
            continue;
        }
        prospective_mms.push(candidate.mm.clone());
        for task in &authorization_tasks {
            let mm = task.with_task_lock_irqsave(|| {
                if !is_global_init_or_kthread(&leader_of(task.clone()))
                    && task.sig_info_irqsave().oom_score_adj() != OOM_SCORE_ADJ_MIN
                    && group
                        .as_ref()
                        .is_some_and(|group| account_for_task(task).is_descendant_of(group))
                {
                    task.basic().user_vm()
                } else {
                    None
                }
            });
            if let Some(mm) = mm {
                prospective_mms.push(mm);
            }
        }
        // A CLONE_VM TGID published during preparation must also have signal
        // storage before any kill/reap. Unrelated new independent MMs do not
        // force this retry. Fork publication is blocked until commit ends;
        // an exec can introduce only its fresh, independently born MM.
        let missing_target = authorization_tasks.iter().any(|task| {
            task.raw_tgid() != victim_tgid
                && !is_global_init_or_kthread(&leader_of(task.clone()))
                && !secondary.iter().any(|kill| kill.tgid == task.raw_tgid())
                && task.with_task_lock_irqsave(|| {
                    prospective_mms.iter().any(|mm| task_uses_mm(task, mm))
                })
        });
        if missing_target {
            drop(membership);
            drop(adj_guard);
            if preparation_retries == 0 {
                return Err(SystemError::ESRCH);
            }
            preparation_retries -= 1;
            continue;
        }
        let mut primary_event = None;
        let committed = primary.commit_if_locked(
            |target| {
                task_uses_mm(target, &candidate.mm)
                    && target.raw_tgid() == candidate.tgid
                    && Arc::ptr_eq(&account_for_task(target), &victim_account)
                    && !should_skip_candidate(target, target.sig_info_irqsave().oom_score_adj())
                    && scope.is_none_or(|scope| {
                        scope.is_online()
                            && account_for_task(target).is_descendant_of(scope)
                            && scope.policy().max.is_some_and(|max| {
                                scope.snapshot().current.saturating_add(requested_pages) > max
                            })
                    })
            },
            |target| {
                killed_mms.push(candidate.mm.clone());
                primary_event = Some(candidate.mm.memory_account());
                if let Some(retired) = target
                    .process_signal()
                    .record_oom_victim_mm(victim_tgid, &candidate.mm)
                {
                    retired_mms.push(retired);
                }
                if scope.is_none() {
                    OOM_STATE.lock_irqsave().inflight = Some(OomVictimState {
                        generation,
                        tgid: victim_tgid,
                        mm_id: candidate.mm.id(),
                    });
                }
            },
        );
        if !committed {
            drop(membership);
            drop(adj_guard);
            return Err(SystemError::ESRCH);
        }
        // First commit still-valid group members and retain their actual mm.
        for kill in &mut secondary {
            let mut event_account = None;
            kill.committed = kill.signal.commit_if_locked(
                |target| {
                    target.raw_tgid() == kill.tgid
                        && !is_global_init_or_kthread(&leader_of(target.clone()))
                        && target.basic().user_vm().is_some()
                        && target.sig_info_irqsave().oom_score_adj() != OOM_SCORE_ADJ_MIN
                        && group
                            .as_ref()
                            .is_some_and(|group| account_for_task(target).is_descendant_of(group))
                },
                |target| {
                    if let Some(mm) = target.basic().user_vm() {
                        event_account = Some(mm.memory_account());
                        if let Some(retired) = target
                            .process_signal()
                            .record_oom_victim_mm(target.raw_tgid(), &mm)
                        {
                            retired_mms.push(retired);
                        }
                        killed_mms.push(mm);
                    }
                },
            );
            kill.event_account = event_account;
        }
        // Only actual killed mm authorize the Linux cross-TGID exception.
        // -1000 protects ordinary group victims, not shared-mm cleanup.
        for kill in &mut secondary {
            if kill.committed {
                continue;
            }
            kill.committed = kill.signal.commit_if_locked(
                |target| {
                    target.raw_tgid() == kill.tgid
                        && !is_global_init_or_kthread(&leader_of(target.clone()))
                        && killed_mms.iter().any(|mm| task_uses_mm(target, mm))
                },
                |_| {},
            );
        }
        // Use a fresh full snapshot from this publication transaction, not
        // the earlier preparation snapshot. A just-published init thread may
        // retain an old MM after its parent exec/exit. Inspect its actual MM
        // under task_lock; PID membership blocks another fork publication.
        // Exec installs only its newly born MM, never one of these killed MMs.
        for mm in &killed_mms {
            let shared_init = authorization_tasks.iter().any(|task| {
                task.raw_tgid().data() == 1
                    && task.with_task_lock_irqsave(|| task_uses_mm(task, mm))
            });
            if shared_init {
                mm.oom_reap_slot().deny_before_execution();
            } else {
                super::oom_reaper::queue_committed_mm(mm, OomReapAuthorization { mm_id: mm.id() });
            }
        }
        drop(membership);
        drop(adj_guard);
        drop(retired_mms);
        for mm in &killed_mms {
            if matches!(
                mm.oom_reap_slot().status(),
                super::oom_reaper::ReapStatus::Finished(_)
            ) {
                note_oom_reaping_finished(mm.id());
            }
        }
        super::oom_reaper::wake_worker();
        if let Some(account) = primary_event {
            account.record_event(MemoryEvent::OomKill);
        }
        primary.dispatch();
        for kill in secondary {
            if kill.committed {
                if let Some(account) = kill.event_account {
                    account.record_event(MemoryEvent::OomKill);
                    count_oom_kill();
                }
                kill.signal.dispatch();
            }
        }
        if let Some(group) = group {
            group.record_event(MemoryEvent::OomGroupKill);
        }
        return Ok(CommittedOomVictim {
            tgid: victim_tgid,
            identity: MemoryOomVictim {
                mm: Arc::downgrade(&candidate.mm),
                mm_id: candidate.mm.id(),
            },
        });
    }
}

/// Whether the current task is marked as an OOM victim.
pub fn current_is_oom_victim() -> bool {
    if !ProcessManager::initialized() {
        return false;
    }
    let current = ProcessManager::current_pcb();
    current
        .process_signal()
        .oom_victim_mm_matches(current.raw_tgid())
        && current_is_killed_or_exiting()
}

fn wait_for_oom_slot() -> Result<(), SystemError> {
    OOM_WAITQ.wait_event_killable(
        || {
            if current_is_killed_or_exiting() {
                return true;
            }
            let state = OOM_STATE.lock_irqsave();
            if state.selecting {
                return false;
            }
            state.inflight.as_ref().is_none()
        },
        None::<fn()>,
    )
}

fn wait_until_recoverable(generation: u64) -> Result<(), SystemError> {
    OOM_WAITQ.wait_event_killable(
        || {
            if current_is_killed_or_exiting() {
                return true;
            }
            let state = OOM_STATE.lock_irqsave();
            if state.selecting {
                return false;
            }
            match state.inflight.as_ref() {
                None => true,
                Some(victim) if victim.generation == generation => false,
                Some(_) => true,
            }
        },
        None::<fn()>,
    )
}

pub fn pagefault_out_of_memory(ctx: OomContext) -> OomOutcome {
    loop {
        if current_is_killed_or_exiting() {
            return OomOutcome::CurrentTaskKilled;
        }

        let generation = match begin_selection(false) {
            Ok(generation) => generation,
            Err(()) => {
                let _ = wait_for_oom_slot();
                continue;
            }
        };

        let candidate = match select_victim(None) {
            Ok(OomSelection::Candidate(candidate)) => candidate,
            Ok(OomSelection::None | OomSelection::ExistingVictim(_)) | Err(_) => {
                finish_selection_none();
                error!(
                    "oom: no victim for trigger pid={} tgid={} addr={:#x} ip={:#x}",
                    ctx.trigger_pid,
                    ctx.trigger_tgid,
                    ctx.fault_address.data(),
                    ctx.fault_ip
                );
                return OomOutcome::NoVictim;
            }
        };

        let current = ProcessManager::current_pcb();
        let current_leader = leader_of(current.clone());
        let current_is_victim = candidate.tgid == current_leader.raw_tgid()
            || (task_uses_mm(&current, &candidate.mm)
                && !is_global_init_or_kthread(&current_leader));
        let candidate_tgid = candidate.tgid;
        let victim_score = candidate.score;
        let victim_oom_score_adj = candidate.oom_score_adj;
        let victim_resident_pages = candidate.resident_pages;
        match send_oom_sigkill(generation, &candidate, None, 0) {
            Ok(killed) => {
                finish_selection_none();
                count_oom_kill();
                error!(
                        "oom-kill: trigger_pid={} trigger_tgid={} victim_tgid={} score={} adj={} rss={} order={} addr={:#x} ip={:#x}",
                        ctx.trigger_pid,
                        ctx.trigger_tgid,
                        killed.tgid,
                        victim_score,
                        victim_oom_score_adj,
                        victim_resident_pages,
                        ctx.order,
                        ctx.fault_address.data(),
                        ctx.fault_ip
                );
                drop(candidate);
                if current_is_victim {
                    return OomOutcome::CurrentTaskKilled;
                }
                match wait_until_recoverable(generation) {
                    Ok(()) if current_is_killed_or_exiting() => {
                        return OomOutcome::CurrentTaskKilled
                    }
                    Ok(()) => return OomOutcome::Retry,
                    Err(_) if current_is_killed_or_exiting() => {
                        return OomOutcome::CurrentTaskKilled
                    }
                    Err(_) => return OomOutcome::Retry,
                }
            }
            Err(SystemError::ESRCH) => {
                finish_selection_none();
                continue;
            }
            Err(err) => {
                finish_selection_none();
                warn!(
                    "oom: failed to SIGKILL victim tgid={} for trigger pid={} err={:?}",
                    candidate_tgid, ctx.trigger_pid, err
                );
                return OomOutcome::NoVictim;
            }
        }
    }
}

/// Scoped selection/kill foundation. Recovery waiting and progress notification
/// belong to the allocation slow path and are intentionally not implemented
/// here. A contended selector is explicit Recheck, not a successful retry or a
/// fallback to global OOM. The typed failure keeps the limiting CSS pinned.
pub(crate) fn memory_out_of_memory(
    failure: &ChargeFailure,
    ctx: OomContext,
) -> Result<MemoryOomOutcome, SystemError> {
    if failure.reason != ChargeFailureReason::Max || ctx.order > 3 {
        return Ok(MemoryOomOutcome::NoVictim);
    }
    scoped_out_of_memory(&failure.account, failure.requested_pages, ctx)
}

/// A memory.max writer enforces existing usage, not a failed allocation.
/// Keep its request distinct without fabricating a ChargeFailure/Max event.
pub(crate) fn memory_limit_out_of_memory(
    scope: &Arc<MemoryAccount>,
    ctx: OomContext,
) -> Result<MemoryOomOutcome, SystemError> {
    scoped_out_of_memory(scope, 0, ctx)
}

fn scoped_out_of_memory(
    scope: &Arc<MemoryAccount>,
    requested_pages: u64,
    _ctx: OomContext,
) -> Result<MemoryOomOutcome, SystemError> {
    if current_is_killed_or_exiting() {
        return Ok(MemoryOomOutcome::CurrentTaskKilled);
    }
    if !scope.is_online()
        || scope
            .policy()
            .max
            .is_none_or(|max| scope.snapshot().current.saturating_add(requested_pages) <= max)
    {
        return Ok(MemoryOomOutcome::Recheck);
    }
    let generation = match begin_selection(true) {
        Ok(generation) => generation,
        Err(()) => return Ok(MemoryOomOutcome::Recheck),
    };
    let result = (|| {
        scope.record_event(MemoryEvent::Oom);
        let candidate = match select_victim(Some(scope))? {
            OomSelection::Candidate(candidate) => candidate,
            OomSelection::ExistingVictim(victim) => {
                return Ok(if current_is_killed_or_exiting() {
                    MemoryOomOutcome::CurrentTaskKilled
                } else {
                    MemoryOomOutcome::Victim(victim)
                });
            }
            OomSelection::None => return Ok(MemoryOomOutcome::NoVictim),
        };
        let killed = match send_oom_sigkill(generation, &candidate, Some(scope), requested_pages) {
            Ok(killed) => killed,
            Err(SystemError::ESRCH) => return Ok(MemoryOomOutcome::Recheck),
            Err(error) => return Err(error),
        };
        count_oom_kill();
        Ok(if current_is_killed_or_exiting() {
            MemoryOomOutcome::CurrentTaskKilled
        } else {
            MemoryOomOutcome::Victim(killed.identity)
        })
    })();
    finish_selection_none();
    result
}

pub fn notify_mm_drop(mm_id: u64) {
    if clear_inflight_for_mm(mm_id) {
        wake_oom_waiters();
    }
    crate::mm::memory_notify::request_oom_progress();
}

pub fn should_inject_fault_oom() -> bool {
    let current_tgid = ProcessManager::current_pcb().raw_tgid();
    let mut cfg = OOM_FAULT_INJECT.lock_irqsave();
    if cfg.target_tgid != Some(current_tgid) {
        return false;
    }
    if !cfg.is_enabled() {
        return false;
    }

    let hit = cfg.seen >= cfg.fail_after;
    cfg.seen = cfg.seen.saturating_add(1);
    if !hit {
        return false;
    }

    match cfg.remaining_failures.as_mut() {
        Some(0) => false,
        Some(remaining) => {
            *remaining = remaining.saturating_sub(1);
            true
        }
        None => true,
    }
}

pub fn read_fault_inject_config() -> alloc::string::String {
    let cfg = OOM_FAULT_INJECT.lock_irqsave();
    let target = cfg.target_tgid.map(|pid| pid.data()).unwrap_or(0);
    let remaining = cfg
        .remaining_failures
        .map(|count| count.to_string())
        .unwrap_or_else(|| "persistent".to_string());
    format!(
        "target_tgid={} fail_after={} seen={} remaining={}\n",
        target, cfg.fail_after, cfg.seen, remaining
    )
}

pub fn write_fault_inject_config(data: &[u8]) -> Result<usize, SystemError> {
    let input = core::str::from_utf8(data).map_err(|_| SystemError::EINVAL)?;
    let parts: Vec<&str> = input.split_whitespace().collect();
    if parts.is_empty() {
        return Err(SystemError::EINVAL);
    }

    let target: usize = parts[0].parse().map_err(|_| SystemError::EINVAL)?;
    let mut cfg = OOM_FAULT_INJECT.lock_irqsave();
    if target == 0 {
        *cfg = OomFaultInject::disabled();
        return Ok(data.len());
    }

    let fail_after = parts
        .get(1)
        .copied()
        .unwrap_or("0")
        .parse()
        .map_err(|_| SystemError::EINVAL)?;
    let fail_times: usize = parts
        .get(2)
        .copied()
        .unwrap_or("1")
        .parse()
        .map_err(|_| SystemError::EINVAL)?;

    *cfg = OomFaultInject {
        target_tgid: Some(RawPid::new(target)),
        fail_after,
        seen: 0,
        remaining_failures: if fail_times == 0 {
            None
        } else {
            Some(fail_times)
        },
    };
    Ok(data.len())
}
