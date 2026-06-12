use alloc::vec::Vec;
use alloc::{
    format,
    string::ToString,
    sync::{Arc, Weak},
};
use log::{error, warn};
use system_error::SystemError;

use crate::{
    arch::ipc::signal::Signal,
    ipc::signal_types::{SigCode, SigInfo, SigType},
    libs::{spinlock::SpinLock, wait_queue::WaitQueue},
    process::{pid::PidType, ProcessControlBlock, ProcessFlags, ProcessManager, RawPid},
};

use super::ucontext::AddressSpace;

static OOM_WAITQ: WaitQueue = WaitQueue::default();
static OOM_STATE: SpinLock<OomState> = SpinLock::new(OomState::new());
static OOM_FAULT_INJECT: SpinLock<OomFaultInject> = SpinLock::new(OomFaultInject::disabled());

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

#[derive(Debug, Clone)]
struct OomVictimState {
    generation: u64,
    mm_id: u64,
    mm: Weak<AddressSpace>,
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
    tgid: RawPid,
    leader: Arc<ProcessControlBlock>,
    mm: Arc<AddressSpace>,
    score: usize,
}

fn victim_has_progress(victim: &OomVictimState) -> bool {
    let Some(mm) = victim.mm.upgrade() else {
        return true;
    };
    mm.id() != victim.mm_id || mm.resident_pages() == 0
}

fn wake_oom_waiters() {
    OOM_WAITQ.wake_all();
}

fn current_is_killed() -> bool {
    Signal::fatal_signal_pending(&ProcessManager::current_pcb())
}

fn leader_of(pcb: Arc<ProcessControlBlock>) -> Arc<ProcessControlBlock> {
    ProcessManager::find(pcb.raw_tgid()).unwrap_or(pcb)
}

fn should_skip_candidate(leader: &Arc<ProcessControlBlock>, mm: &Arc<AddressSpace>) -> bool {
    leader.raw_pid().data() == 0
        || leader.raw_pid().data() == 1
        || leader
            .flags()
            .contains(ProcessFlags::KTHREAD | ProcessFlags::EXITING)
        || mm.resident_pages() == 0
}

fn better_candidate(new: &OomCandidate, old: &OomCandidate) -> bool {
    new.score > old.score || (new.score == old.score && new.tgid > old.tgid)
}

fn select_victim() -> Option<OomCandidate> {
    let pids = ProcessManager::get_all_processes();
    let mut seen_tgids = Vec::new();
    let mut best: Option<OomCandidate> = None;

    for pid in pids {
        let Some(task) = ProcessManager::find(pid) else {
            continue;
        };
        let leader = leader_of(task);
        let tgid = leader.raw_tgid();
        if seen_tgids.contains(&tgid) {
            continue;
        }
        seen_tgids.push(tgid);

        let Some(mm) = leader.basic().user_vm() else {
            continue;
        };
        if should_skip_candidate(&leader, &mm) {
            continue;
        }

        let candidate = OomCandidate {
            tgid,
            leader,
            score: mm.resident_pages(),
            mm,
        };
        if best
            .as_ref()
            .is_none_or(|current| better_candidate(&candidate, current))
        {
            best = Some(candidate);
        }
    }

    best
}

fn begin_selection() -> Result<u64, ()> {
    let mut state = OOM_STATE.lock_irqsave();
    if let Some(victim) = state.inflight.as_ref() {
        if victim_has_progress(victim) {
            state.inflight = None;
            wake_oom_waiters();
        }
    }
    if state.selecting || state.inflight.is_some() {
        return Err(());
    }

    state.selecting = true;
    state.generation = state.generation.wrapping_add(1);
    Ok(state.generation)
}

fn finish_selection_none() {
    let mut state = OOM_STATE.lock_irqsave();
    state.selecting = false;
    wake_oom_waiters();
}

fn finish_selection_with_victim(generation: u64, candidate: &OomCandidate) {
    let mut state = OOM_STATE.lock_irqsave();
    state.selecting = false;
    state.inflight = Some(OomVictimState {
        generation,
        mm_id: candidate.mm.id(),
        mm: Arc::downgrade(&candidate.mm),
    });
}

fn send_oom_sigkill(candidate: &OomCandidate) -> Result<(), SystemError> {
    let mut info = SigInfo::new(
        Signal::SIGKILL,
        0,
        SigCode::Kernel,
        SigType::Kill {
            pid: RawPid::new(0),
            uid: 0,
        },
    );
    Signal::SIGKILL
        .send_signal_info_to_pcb(Some(&mut info), candidate.leader.clone(), PidType::TGID)
        .map(|_| ())
}

fn wait_until_recoverable(generation: u64) -> Result<(), SystemError> {
    OOM_WAITQ.wait_event_killable(
        || {
            let state = OOM_STATE.lock_irqsave();
            if state.selecting {
                return false;
            }
            match state.inflight.as_ref() {
                None => true,
                Some(victim) if victim.generation != generation => true,
                Some(victim) => victim_has_progress(victim),
            }
        },
        None::<fn()>,
    )
}

pub fn pagefault_out_of_memory(ctx: OomContext) -> OomOutcome {
    loop {
        if current_is_killed() {
            return OomOutcome::CurrentTaskKilled;
        }

        let generation = match begin_selection() {
            Ok(generation) => generation,
            Err(()) => {
                let _ = wait_until_recoverable(0);
                continue;
            }
        };

        let Some(candidate) = select_victim() else {
            finish_selection_none();
            error!(
                "oom: no victim for trigger pid={} tgid={} addr={:#x} ip={:#x}",
                ctx.trigger_pid,
                ctx.trigger_tgid,
                ctx.fault_address.data(),
                ctx.fault_ip
            );
            return OomOutcome::NoVictim;
        };

        let current_is_victim = candidate.tgid == ctx.trigger_tgid;
        match send_oom_sigkill(&candidate) {
            Ok(()) => {
                error!(
                    "oom-kill: trigger_pid={} trigger_tgid={} victim_tgid={} score={} order={} addr={:#x} ip={:#x}",
                    ctx.trigger_pid,
                    ctx.trigger_tgid,
                    candidate.tgid,
                    candidate.score,
                    ctx.order,
                    ctx.fault_address.data(),
                    ctx.fault_ip
                );
                finish_selection_with_victim(generation, &candidate);
                if current_is_victim {
                    return OomOutcome::CurrentTaskKilled;
                }
                match wait_until_recoverable(generation) {
                    Ok(()) => return OomOutcome::Retry,
                    Err(_) if current_is_killed() => return OomOutcome::CurrentTaskKilled,
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
                    candidate.tgid, ctx.trigger_pid, err
                );
                return OomOutcome::NoVictim;
            }
        }
    }
}

pub fn notify_mm_released(mm: &Arc<AddressSpace>) {
    let mut state = OOM_STATE.lock_irqsave();
    let matches_inflight = state.inflight.as_ref().is_some_and(|victim| {
        victim.mm_id == mm.id()
            || victim
                .mm
                .upgrade()
                .is_some_and(|other| Arc::ptr_eq(&other, mm))
    });
    if matches_inflight {
        state.inflight = None;
    }
    drop(state);
    if matches_inflight {
        wake_oom_waiters();
    }
}

pub fn notify_mm_drop(mm_id: u64) {
    let mut state = OOM_STATE.lock_irqsave();
    if state
        .inflight
        .as_ref()
        .is_some_and(|victim| victim.mm_id == mm_id)
    {
        state.inflight = None;
        wake_oom_waiters();
    }
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
