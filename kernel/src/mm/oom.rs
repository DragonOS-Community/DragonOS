use alloc::{
    format,
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, Ordering};
use system_error::SystemError;

use crate::{
    arch::ipc::signal::Signal,
    cgroup::memory::{
        account_for_task, ChargeFailure, ChargeFailureReason, MemoryAccount, MemoryEvent,
    },
    libs::spinlock::SpinLock,
    process::{ProcessControlBlock, ProcessFlags, ProcessManager, RawPid},
};

use super::ucontext::AddressSpace;

#[cfg(feature = "mm_selftest")]
pub(crate) mod denial;
pub(crate) mod report;
mod workspace;
use workspace::{KilledMm, Lease, TaskRecord, Workspace};

pub(crate) fn reserve_task_capacity(required: usize) -> Result<(), SystemError> {
    workspace::reserve(required)
}
pub(crate) fn has_task_capacity(required: usize) -> bool {
    workspace::has_capacity(required)
}

/// Permission issued only after a committed kill and shared-init exclusion.
/// Keep its field private to this publisher; an MM id alone is not permission.
pub(crate) struct OomReapAuthorization {
    mm_id: u64,
}

impl OomReapAuthorization {
    pub(in crate::mm) fn mm_id(&self) -> u64 {
        self.mm_id
    }

    #[cfg(any(test, feature = "mm_selftest"))]
    pub(in crate::mm) fn test_authorization(mm_id: u64) -> Self {
        Self { mm_id }
    }
}

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
    None(SelectionStats),
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
    #[cfg(feature = "mm_selftest")]
    deny_heap: bool,
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
            #[cfg(feature = "mm_selftest")]
            deny_heap: false,
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
    crate::mm::memory_notify::request_oom_progress();
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
    let total_pages = super::balloon::managed_usage().total().data();
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

#[derive(Debug, Default)]
struct SelectionStats {
    tasks: usize,
    outside_scope: usize,
    no_mm: usize,
    retired_mm: usize,
    protected: usize,
    exiting: usize,
    vfork: usize,
    eligible: usize,
}

fn select_victim(records: &mut [TaskRecord], scope: Option<&Arc<MemoryAccount>>) -> OomSelection {
    let total_pages = scope.map_or_else(total_system_pages, |scope| {
        scope
            .policy()
            .max
            .unwrap_or(1)
            .max(1)
            .min(isize::MAX as u64) as isize
    });
    let mut stats = SelectionStats::default();
    let mut best: Option<OomCandidate> = None;
    for record in records {
        stats.tasks += 1;
        let task = &record.task;
        if scope.is_some_and(|scope| !account_for_task(task).is_descendant_of(scope)) {
            stats.outside_scope += 1;
            continue;
        }
        let leader = leader_of(task.clone());
        let tgid = record.tgid;
        if scope.is_some() {
            if is_global_init_or_kthread(&leader) {
                stats.protected += 1;
                continue;
            }
            if let Some((mm_id, mm)) = leader.process_signal().recorded_oom_victim(tgid) {
                // Keep any upgraded observer until after the IRQ transaction.
                record.observed_oom_mm = mm.upgrade();
                let unfinished = record.observed_oom_mm.as_ref().is_some_and(|mm| {
                    !mm.is_torn_down()
                        && !matches!(
                            mm.oom_reap_slot().status(),
                            super::oom_reaper::ReapStatus::Finished(_)
                        )
                });
                if unfinished {
                    return OomSelection::ExistingVictim(MemoryOomVictim { mm, mm_id });
                }
                stats.retired_mm += 1;
                continue;
            }
        }
        let Some(mm) = record.observed_mm.as_ref() else {
            stats.no_mm += 1;
            continue;
        };
        if mm.is_torn_down()
            || matches!(
                mm.oom_reap_slot().status(),
                super::oom_reaper::ReapStatus::Finished(_)
            )
        {
            stats.retired_mm += 1;
            continue;
        }
        let oom_score_adj = leader.sig_info_irqsave().oom_score_adj();
        if is_global_init_or_kthread(&leader) || oom_score_adj == OOM_SCORE_ADJ_MIN {
            stats.protected += 1;
            continue;
        }
        if task.flags().contains(ProcessFlags::EXITING) {
            stats.exiting += 1;
            continue;
        }
        if task.is_active_vfork() {
            stats.vfork += 1;
            continue;
        }
        // Considering each eligible TID is idempotent for a stable group and
        // does not let an exited/mm-less thread hide a live worker.
        stats.eligible += 1;
        let candidate = OomCandidate {
            task: task.clone(),
            tgid,
            mm: mm.clone(),
            score: if scope.is_some() {
                scoped_badness(
                    mm.resident_pages(),
                    mm.page_table_pages(),
                    oom_score_adj,
                    total_pages,
                )
            } else {
                oom_score(mm, oom_score_adj, total_pages)
            },
            resident_pages: mm.resident_pages(),
            oom_score_adj,
        };
        if best
            .as_ref()
            .is_none_or(|old| better_candidate(&candidate, old))
        {
            best = Some(candidate);
        }
    }
    best.map_or(OomSelection::None(stats), OomSelection::Candidate)
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

/// Return the end of a contiguous TGID group in the prepared task snapshot.
fn group_end(records: &[TaskRecord], start: usize) -> usize {
    let tgid = records[start].tgid;
    start + records[start..].partition_point(|record| record.tgid == tgid)
}

/// Only the task-locked actual commit can create an MM reaping permission.
fn commit_group(records: &mut [TaskRecord], target: usize, mm: Option<&Arc<AddressSpace>>) {
    let task = records[target].task.clone();
    task.process_signal()
        .start_group_exit_for_fatal_signal(Signal::SIGKILL as usize);
    if let Some(mm) = mm {
        assert!(
            records[target].retired_mm.is_none(),
            "each OOM group commits once"
        );
        records[target].retired_mm = task
            .process_signal()
            .record_oom_victim_mm(records[target].tgid, mm);
        records[target].killed_mm = Some(mm.clone());
    }
    for record in records {
        record.dispatch = true;
        #[cfg(feature = "mm_selftest")]
        denial::mark_victim(&record.task);
    }
}

fn mm_index(mms: &[KilledMm], mm: &Arc<AddressSpace>) -> Option<usize> {
    mms.binary_search_by_key(&mm.id(), |entry| entry.mm.id())
        .ok()
}

/// Called once with adjustment and PID membership serialized. All backing was
/// prepared before task publication; no signal dispatch/owner retirement here.
fn commit_oom_kill(
    generation: u64,
    candidate: &OomCandidate,
    scope: Option<&Arc<MemoryAccount>>,
    requested_pages: u64,
    work: &mut Workspace,
) -> Result<CommittedOomVictim, SystemError> {
    let victim_account = account_for_task(&candidate.task);
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
    let primary = work
        .tasks
        .iter()
        .position(|r| Arc::ptr_eq(&r.task, &candidate.task))
        .ok_or(SystemError::ESRCH)?;
    let start = work
        .tasks
        .partition_point(|r| r.tgid.data() < candidate.tgid.data());
    let end = group_end(&work.tasks, start);
    let target = candidate.task.clone();
    let committed = target.with_task_lock_irqsave(|| {
        if target.raw_tgid() != candidate.tgid
            || !task_uses_mm(&target, &candidate.mm)
            || !Arc::ptr_eq(&account_for_task(&target), &victim_account)
            || should_skip_candidate(
                &target,
                leader_of(target.clone()).sig_info_irqsave().oom_score_adj(),
            )
            || scope.is_some_and(|scope| {
                !scope.is_online()
                    || !account_for_task(&target).is_descendant_of(scope)
                    || scope.policy().max.is_none_or(|max| {
                        scope.snapshot().current.saturating_add(requested_pages) <= max
                    })
            })
        {
            return false;
        }
        commit_group(
            &mut work.tasks[start..end],
            primary - start,
            Some(&candidate.mm),
        );
        if scope.is_none() {
            OOM_STATE.lock_irqsave().inflight = Some(OomVictimState {
                generation,
                tgid: candidate.tgid,
                mm_id: candidate.mm.id(),
            });
        }
        true
    });
    if !committed {
        return Err(SystemError::ESRCH);
    }

    // Ordinary group victims retain -1000 protection. Visit each group once;
    // a live worker may own its MM even if the leader has already exited.
    let mut start = 0;
    while start < work.tasks.len() {
        let end = group_end(&work.tasks, start);
        if !work.tasks[start].dispatch {
            for index in start..end {
                let target = work.tasks[index].task.clone();
                let committed = target.with_task_lock_irqsave(|| {
                    if is_global_init_or_kthread(&leader_of(target.clone()))
                        || target.is_exited()
                        || leader_of(target.clone()).sig_info_irqsave().oom_score_adj()
                            == OOM_SCORE_ADJ_MIN
                        || !group
                            .as_ref()
                            .is_some_and(|group| account_for_task(&target).is_descendant_of(group))
                    {
                        return false;
                    }
                    // Basic guard outlives the temporary MM owner on rejection.
                    let basic = target.basic();
                    let Some(mm) = basic.user_vm() else {
                        return false;
                    };
                    commit_group(&mut work.tasks[start..end], index - start, Some(&mm));
                    true
                });
                if committed {
                    break;
                }
            }
        }
        start = end;
    }
    for record in &work.tasks {
        if let Some(mm) = &record.killed_mm {
            work.mms.push(KilledMm {
                mm: mm.clone(),
                shared_init: false,
            });
        }
    }
    work.mms.sort_unstable_by_key(|entry| entry.mm.id());
    // Every actual committed MM is still pinned by its TaskRecord. Removing
    // duplicate set owners therefore cannot run MM destruction under IRQ locks.
    work.mms.dedup_by_key(|entry| entry.mm.id());

    // Linux's shared-MM exception can kill a protected sibling TGID, but can
    // never authorize reaping a different image or a still-shared init MM.
    let mut start = 0;
    while start < work.tasks.len() {
        let end = group_end(&work.tasks, start);
        if !work.tasks[start].dispatch {
            for index in start..end {
                let target = work.tasks[index].task.clone();
                let committed = target.with_task_lock_irqsave(|| {
                    if is_global_init_or_kthread(&leader_of(target.clone())) {
                        return false;
                    }
                    let basic = target.basic();
                    let shares = basic
                        .user_vm()
                        .is_some_and(|mm| mm_index(&work.mms, &mm).is_some());
                    if shares {
                        commit_group(&mut work.tasks[start..end], index - start, None);
                    }
                    shares
                });
                if committed {
                    break;
                }
            }
        }
        start = end;
    }
    for record in &work.tasks {
        if record.tgid.data() == 1 {
            record.task.with_task_lock_irqsave(|| {
                let basic = record.task.basic();
                if let Some(mm) = basic.user_vm() {
                    if let Some(index) = mm_index(&work.mms, &mm) {
                        work.mms[index].shared_init = true;
                    }
                }
            });
        }
    }
    for entry in &work.mms {
        if entry.shared_init {
            entry.mm.oom_reap_slot().deny_before_execution();
        } else {
            super::oom_reaper::queue_committed_mm(
                &entry.mm,
                OomReapAuthorization {
                    mm_id: entry.mm.id(),
                },
            );
        }
    }
    if let Some(group) = group {
        // Store no new owner in the work queue. Account events are counters.
        group.record_event(MemoryEvent::OomGroupKill);
    }
    Ok(CommittedOomVictim {
        tgid: candidate.tgid,
        identity: MemoryOomVictim {
            mm: Arc::downgrade(&candidate.mm),
            mm_id: candidate.mm.id(),
        },
    })
}

/// All registry/publication/adjustment guards are gone before scheduling or
/// retiring the snapshots. GROUP_EXIT already prevents late thread publication.
fn dispatch_oom_kill(work: &Workspace) {
    super::oom_reaper::wake_worker();
    for entry in &work.mms {
        if matches!(
            entry.mm.oom_reap_slot().status(),
            super::oom_reaper::ReapStatus::Finished(_)
        ) {
            note_oom_reaping_finished(entry.mm.id());
        }
    }
    for record in &work.tasks {
        if let Some(mm) = &record.killed_mm {
            mm.memory_account().record_event(MemoryEvent::OomKill);
            count_oom_kill();
        }
        if record.dispatch {
            Signal::queue_private_sigkill_to_thread(&record.task);
        }
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
    crate::libs::resource_wait::wait_until_killable(|| {
        if current_is_killed_or_exiting() {
            return Some(());
        }
        let state = OOM_STATE.lock_irqsave();
        if state.selecting {
            return None;
        }
        state.inflight.as_ref().is_none().then_some(())
    })
}

fn wait_until_recoverable(generation: u64) -> Result<(), SystemError> {
    crate::libs::resource_wait::wait_until_killable(|| {
        if current_is_killed_or_exiting() {
            return Some(());
        }
        let state = OOM_STATE.lock_irqsave();
        if state.selecting {
            return None;
        }
        match state.inflight.as_ref() {
            None => Some(()),
            Some(victim) if victim.generation == generation => None,
            Some(_) => Some(()),
        }
    })
}

pub fn pagefault_out_of_memory(ctx: OomContext) -> OomOutcome {
    #[cfg(feature = "mm_selftest")]
    let _heap_scope = denial::Scope::enter(test_heap_denial_requested());
    #[cfg(feature = "mm_selftest")]
    if test_heap_denial_requested() {
        assert!(denial::probe_denied(), "OOM heap-denial probes are active");
    }
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
        // Global selection is serialized, but no victim/PID/MM resource is
        // locked yet. Balloon completion may sleep and must precede a kill.
        if crate::driver::virtio::virtio_balloon::deflate_on_oom() {
            finish_selection_none();
            return OomOutcome::Retry;
        }
        let mut work = Lease::checkout();
        let adjustment = match ProcessManager::lock_oom_score_adj_for_oom() {
            Ok(guard) => guard,
            Err(_) => {
                drop(work);
                finish_selection_none();
                return OomOutcome::CurrentTaskKilled;
            }
        };
        let membership = crate::process::pid::pid_membership_lock();
        work.fill();
        let candidate = match select_victim(&mut work.tasks, None) {
            OomSelection::Candidate(candidate) => candidate,
            OomSelection::None(stats) => {
                drop(membership);
                drop(adjustment);
                drop(work);
                finish_selection_none();
                report::report(format_args!(
                    "oom: no eligible victim pid={} tgid={} addr={:#x} ip={:#x} filters={:?}\n",
                    ctx.trigger_pid,
                    ctx.trigger_tgid,
                    ctx.fault_address.data(),
                    ctx.fault_ip,
                    stats
                ));
                return OomOutcome::NoVictim;
            }
            OomSelection::ExistingVictim(_) => unreachable!("recorded-victim lookup is scoped"),
        };
        let current = ProcessManager::current_pcb();
        let current_leader = leader_of(current.clone());
        let current_is_victim = candidate.tgid == current_leader.raw_tgid()
            || (task_uses_mm(&current, &candidate.mm)
                && !is_global_init_or_kthread(&current_leader));
        let result = commit_oom_kill(generation, &candidate, None, 0, &mut work);
        drop(membership);
        drop(adjustment);
        match result {
            Ok(killed) => {
                dispatch_oom_kill(&work);
                report::report(format_args!(
                    "oom-kill: trigger_pid={} trigger_tgid={} victim_tgid={} score={} adj={} rss={} order={} addr={:#x} ip={:#x}\n",
                    ctx.trigger_pid, ctx.trigger_tgid, killed.tgid, candidate.score,
                    candidate.oom_score_adj, candidate.resident_pages, ctx.order,
                    ctx.fault_address.data(), ctx.fault_ip));
                drop(candidate);
                drop(work);
                finish_selection_none();
                if current_is_victim {
                    return OomOutcome::CurrentTaskKilled;
                }
                let _ = wait_until_recoverable(generation);
                return if current_is_killed_or_exiting() {
                    OomOutcome::CurrentTaskKilled
                } else {
                    OomOutcome::Retry
                };
            }
            Err(SystemError::ESRCH) => {
                // An exec or policy transition invalidated the actual commit;
                // this is not an allocation failure or evidence of no victim.
                drop(candidate);
                drop(work);
                finish_selection_none();
            }
            Err(_) => unreachable!("OOM commit has no resource allocation failure"),
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
    let mut work = Lease::checkout();
    let adjustment = match ProcessManager::lock_oom_score_adj_for_oom() {
        Ok(guard) => guard,
        Err(error) => {
            drop(work);
            finish_selection_none();
            return Err(error);
        }
    };
    let membership = crate::process::pid::pid_membership_lock();
    work.fill();
    scope.record_event(MemoryEvent::Oom);
    let mut committed = false;
    let result = match select_victim(&mut work.tasks, Some(scope)) {
        OomSelection::Candidate(candidate) => {
            let killed = commit_oom_kill(
                generation,
                &candidate,
                Some(scope),
                requested_pages,
                &mut work,
            );
            // The workspace pins candidate's task and observed MM until guards
            // are gone, so dropping this extra owner cannot retire the MM here.
            match killed {
                Ok(killed) => {
                    committed = true;
                    Ok(MemoryOomOutcome::Victim(killed.identity))
                }
                Err(SystemError::ESRCH) => Ok(MemoryOomOutcome::Recheck),
                Err(error) => Err(error),
            }
        }
        OomSelection::ExistingVictim(victim) => Ok(MemoryOomOutcome::Victim(victim)),
        OomSelection::None(_) => Ok(MemoryOomOutcome::NoVictim),
    };
    drop(membership);
    drop(adjustment);
    if committed {
        dispatch_oom_kill(&work);
    }
    drop(work);
    finish_selection_none();
    if current_is_killed_or_exiting() {
        Ok(MemoryOomOutcome::CurrentTaskKilled)
    } else {
        result
    }
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
    let output = format!(
        "target_tgid={} fail_after={} seen={} remaining={}\n",
        target, cfg.fail_after, cfg.seen, remaining
    );
    #[cfg(feature = "mm_selftest")]
    let output = output + &format!(
        "heap_denial_supported=1 heap_rejections={} task_record_bytes={} killed_mm_bytes={} pcb_wait_node_bytes={} mm_drop_node_bytes={} rwsem_lock_bytes={}\n",
        denial::rejected_count(),
        core::mem::size_of::<TaskRecord>(),
        core::mem::size_of::<KilledMm>(),
        core::mem::size_of::<SpinLock<crate::libs::resource_wait::ResourceWaitLink>>(),
        core::mem::size_of::<SpinLock<super::lazy_mm_drop::LazyMmDropLink>>(),
        core::mem::size_of::<crate::libs::rwsem::RwSem<()>>(),
    );
    output
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

    #[cfg(feature = "mm_selftest")]
    let deny_heap = match parts.get(3).copied().unwrap_or("0") {
        "0" => false,
        "1" => true,
        _ => return Err(SystemError::EINVAL),
    };
    #[cfg(not(feature = "mm_selftest"))]
    if parts.len() > 3 {
        return Err(SystemError::EINVAL);
    }
    if parts.len() > 4 {
        return Err(SystemError::EINVAL);
    }
    *cfg = OomFaultInject {
        #[cfg(feature = "mm_selftest")]
        deny_heap,
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

#[cfg(feature = "mm_selftest")]
fn test_heap_denial_requested() -> bool {
    let cfg = OOM_FAULT_INJECT.lock_irqsave();
    cfg.deny_heap && cfg.target_tgid == Some(ProcessManager::current_pcb().raw_tgid())
}
