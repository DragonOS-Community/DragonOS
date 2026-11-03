//! Real UMH exec lifecycle checks, built only with `mm_selftest`.
//!
//! Observation is scoped to the exact helper PCB, never to a pathname or all
//! exec callers. Strong MM references only retain storage, not user counts.

use alloc::{
    ffi::CString,
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, Ordering};
use system_error::SystemError;

use crate::{
    arch::{CurrentIrqArch, MMArch},
    exception::InterruptArch,
    libs::spinlock::SpinLock,
    mm::{ucontext::AddressSpace, MemoryManagementArch, PageTableKind},
    process::{ProcessControlBlock, ProcessManager},
};

use super::UserModeHelper;

struct Round {
    mm: Arc<AddressSpace>,
    restored: bool,
    restarted: bool,
    restored_root: bool,
}

struct State {
    valid: bool,
    entered: bool,
    prepared: Option<bool>,
    rounds: Vec<Round>,
}

pub(super) struct Observation {
    state: SpinLock<State>,
}

impl Observation {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: SpinLock::new(State {
                valid: true,
                entered: false,
                prepared: None,
                // More than the bounded interpreter recursion limit. No
                // allocation is needed while recording an installed MM.
                rounds: Vec::with_capacity(16),
            }),
        })
    }
}

struct Active {
    child: Weak<ProcessControlBlock>,
    observation: Arc<Observation>,
}

static ACTIVE: SpinLock<Option<Active>> = SpinLock::new(None);
static RUNNING: AtomicBool = AtomicBool::new(false);

pub(super) struct ChildObservation(Option<Arc<Observation>>);

impl Drop for ChildObservation {
    fn drop(&mut self) {
        let Some(observation) = &self.0 else { return };
        let removed = {
            let mut active = ACTIVE.lock_irqsave();
            if active
                .as_ref()
                .is_some_and(|entry| Arc::ptr_eq(&entry.observation, observation))
            {
                active.take()
            } else {
                None
            }
        };
        drop(removed);
    }
}

pub(super) fn observe_child(
    child: &Arc<ProcessControlBlock>,
    observation: Option<Arc<Observation>>,
) -> Result<ChildObservation, SystemError> {
    let Some(observation) = observation else {
        return Ok(ChildObservation(None));
    };
    {
        let mut state = observation.state.lock_irqsave();
        state.entered = true;
        state.valid &= child.basic().user_vm().is_none();
    }
    let mut active = ACTIVE.lock_irqsave();
    if active.is_some() {
        return Err(SystemError::EBUSY);
    }
    *active = Some(Active {
        child: Arc::downgrade(child),
        observation: observation.clone(),
    });
    Ok(ChildObservation(Some(observation)))
}

fn current_observation() -> Option<Arc<Observation>> {
    let current = ProcessManager::current_pcb();
    let active = ACTIVE.lock_irqsave();
    let entry = active.as_ref()?;
    if Weak::ptr_eq(&entry.child, &Arc::downgrade(&current)) {
        Some(entry.observation.clone())
    } else {
        None
    }
}

fn root_matches(mm: &Arc<AddressSpace>) -> bool {
    let _irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
    let cpu = crate::smp::core::smp_get_processor_id();
    (unsafe { MMArch::table(PageTableKind::User) }) == mm.table_paddr()
        && crate::mm::tlb::tlb_state_force(cpu).loaded_is(mm)
        && mm.active_cpus.lock().get(cpu).unwrap_or(false)
}

pub(crate) fn installed(mm: &Arc<AddressSpace>, old: Option<&Arc<AddressSpace>>) {
    let Some(observation) = current_observation() else {
        return;
    };
    let current = ProcessManager::current_pcb().basic().user_vm();
    let valid = old.is_none()
        && current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, mm))
        && mm.user_count() == 1
        && !mm.is_torn_down()
        && root_matches(mm);
    let mut state = observation.state.lock_irqsave();
    let previous_complete = state
        .rounds
        .last()
        .is_none_or(|round| round.restored && round.restarted);
    state.valid &= valid && previous_complete;
    if state.rounds.len() == state.rounds.capacity() {
        state.valid = false;
        return;
    }
    state.rounds.push(Round {
        mm: mm.clone(),
        restored: false,
        restarted: false,
        restored_root: false,
    });
}

// Called while the actual Option installation still has IRQs disabled. Once
// mmput can schedule, a None task may legitimately borrow another root.
pub(crate) fn restoring_none_root(target: &Arc<AddressSpace>) {
    let Some(observation) = current_observation() else {
        return;
    };
    let valid =
        Arc::ptr_eq(target, &crate::mm::IDLE_PROCESS_ADDRESS_SPACE()) && root_matches(target);
    let mut state = observation.state.lock_irqsave();
    state.valid &= valid;
    if let Some(round) = state.rounds.last_mut() {
        round.restored_root = valid;
    } else {
        state.valid = false;
    }
}

pub(crate) fn restored(mm: &Arc<AddressSpace>, restarting: bool, post_point_of_no_return: bool) {
    let Some(observation) = current_observation() else {
        return;
    };
    let valid = ProcessManager::current_pcb().basic().user_vm().is_none()
        && mm.user_count() == 0
        && mm.is_torn_down()
        && !post_point_of_no_return;
    let mut state = observation.state.lock_irqsave();
    state.valid &= valid;
    match state.rounds.last_mut() {
        Some(round) if Arc::ptr_eq(&round.mm, mm) && !round.restored && round.restored_root => {
            round.restored = true;
            round.restarted = restarting;
        }
        _ => state.valid = false,
    }
}

pub(super) fn prepared(success: bool) {
    let Some(observation) = current_observation() else {
        return;
    };
    let current = ProcessManager::current_pcb().basic().user_vm();
    let root_valid = if let Some(mm) = current.as_ref() {
        success && mm.user_count() == 1 && !mm.is_torn_down() && root_matches(mm)
    } else {
        // Keep the lookup and hardware qualification on the same CPU even
        // when no prospective MM was ever installed (open-time failure).
        let _irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
        !success
            && crate::mm::tlb::tlb_state_loaded_mm()
                .as_ref()
                .is_some_and(root_matches)
    };
    let mut state = observation.state.lock_irqsave();
    state.valid &= root_valid;
    if success {
        let final_installed = state.rounds.last().is_some_and(|round| {
            !round.restored
                && current
                    .as_ref()
                    .is_some_and(|mm| Arc::ptr_eq(mm, &round.mm))
        });
        state.valid &= final_installed;
    }
    state.prepared = Some(success);
}

struct RunningGuard;
impl Drop for RunningGuard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::Release);
    }
}

pub(super) fn run() -> Result<String, SystemError> {
    if RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(SystemError::EBUSY);
    }
    let _running = RunningGuard;
    let cases = [
        ("elf", "/bin/busybox", Some("true"), Ok(0), 1),
        (
            "missing_path",
            "/no/such/usermode-helper",
            None,
            Err(SystemError::ENOENT),
            0,
        ),
        (
            "invalid_elf",
            "/tmp/null-mm-exec/invalid",
            None,
            Err(SystemError::ENOEXEC),
            1,
        ),
        (
            "shebang",
            "/tmp/null-mm-exec/script-ok",
            None,
            Ok(37 << 8),
            2,
        ),
        (
            "missing_interpreter",
            "/tmp/null-mm-exec/script-missing",
            None,
            Err(SystemError::ENOENT),
            1,
        ),
        (
            "invalid_interpreter",
            "/tmp/null-mm-exec/script-invalid-interpreter",
            None,
            Err(SystemError::ENOEXEC),
            2,
        ),
    ];
    let mut report = String::new();
    for (name, path, argument, expected, rounds) in cases {
        let observation = Observation::new();
        let mut argv = vec![CString::new(path).map_err(|_| SystemError::EINVAL)?];
        if let Some(argument) = argument {
            argv.push(CString::new(argument).map_err(|_| SystemError::EINVAL)?);
        }
        let result = UserModeHelper::start_internal(
            path.into(),
            argv,
            Vec::new(),
            None,
            None,
            Some(observation.clone()),
        )?
        .wait();
        let state = observation.state.lock_irqsave();
        let success = expected.is_ok();
        let qualified = result == expected
            && state.entered
            && state.valid
            && state.prepared == Some(success)
            && state.rounds.len() == rounds
            && state.rounds.iter().enumerate().all(|(index, round)| {
                round.mm.user_count() == 0
                    && round.mm.is_torn_down()
                    && if index + 1 < rounds {
                        round.restored && round.restarted
                    } else {
                        round.restored != success && !round.restarted
                    }
            });
        drop(state);
        if !qualified {
            return Err(SystemError::EIO);
        }
        report.push_str(&format!("null_mm_{name}=ok rounds={rounds} reaped=1\n"));
    }
    // No injected failure is represented as a naturally observed OOM.
    report.push_str("null_mm_natural_oom=not_tested\n");
    Ok(report)
}
