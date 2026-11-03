//! Real userspace exit while a naturally scheduled NULL-mm task holds its root.
//! This fixture never installs an mm or changes the shootdown protocol.

use alloc::{
    boxed::Box,
    ffi::CString,
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    hint::spin_loop,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
use system_error::SystemError;

use crate::{
    arch::{CurrentIrqArch, MMArch},
    exception::InterruptArch,
    libs::{cpumask::CpuMask, mutex::Mutex, spinlock::SpinLock},
    mm::{ucontext::AddressSpace, MemoryManagementArch, PageTableKind},
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        preempt::PreemptGuard,
        usermodehelper::UserModeHelper,
        ProcessControlBlock, ProcessManager,
    },
    sched::completion::Completion,
    smp::{
        core::smp_get_processor_id,
        cpu::{smp_cpu_manager, ProcessorId},
    },
    time::{clocksource::HZ, timer::clock},
};

const HELPER: &str = "/opt/tests/dunitest/bin/normal/null_mm_lifetime_test";
const BUDGET: u64 = 5 * HZ;
const MAX_SPINS: usize = 200_000_000;
static SERIAL: Mutex<()> = Mutex::new(());
static SESSION: SpinLock<Option<Arc<Session>>> = SpinLock::new(None);

struct Session {
    source: ProcessorId,
    target: ProcessorId,
    child: SpinLock<Weak<ProcessControlBlock>>,
    mm: SpinLock<Weak<AddressSpace>>,
    worker: SpinLock<Option<Arc<ProcessControlBlock>>>,
    armed: AtomicBool,
    started: AtomicBool,
    admitted: AtomicBool,
    cancelled: AtomicBool,
    reaped: AtomicBool,
    reaped_ok: AtomicBool,
    passed: AtomicBool,
    no_users: AtomicBool,
    torn_down: AtomicBool,
    root_held: AtomicBool,
    ready: Completion,
    child_done: Completion,
    initial_gen: AtomicU64,
    final_gen: AtomicU64,
    loaded_gen: AtomicU64,
}

impl Session {
    fn new(source: ProcessorId, target: ProcessorId) -> Self {
        Self {
            source,
            target,
            child: SpinLock::new(Weak::new()),
            mm: SpinLock::new(Weak::new()),
            worker: SpinLock::new(None),
            armed: AtomicBool::new(false),
            started: AtomicBool::new(false),
            admitted: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            reaped: AtomicBool::new(false),
            reaped_ok: AtomicBool::new(false),
            passed: AtomicBool::new(false),
            no_users: AtomicBool::new(false),
            torn_down: AtomicBool::new(false),
            root_held: AtomicBool::new(false),
            ready: Completion::new(),
            child_done: Completion::new(),
            initial_gen: AtomicU64::new(0),
            final_gen: AtomicU64::new(0),
            loaded_gen: AtomicU64::new(0),
        }
    }

    fn is_child(&self, child: &Arc<ProcessControlBlock>) -> bool {
        let expected = { self.child.lock_irqsave().upgrade() };
        expected.is_some_and(|expected| Arc::ptr_eq(&expected, child))
    }
}

fn current_session() -> Option<Arc<Session>> {
    SESSION.lock_irqsave().clone()
}

/// Only the exact helper created by this fixture gets the control-file open.
/// Called before exec by the existing UMH child preparation path.
pub(crate) fn prepare_helper(
    child: &Arc<ProcessControlBlock>,
    path: &str,
) -> Result<(), SystemError> {
    if path != HELPER {
        return Ok(());
    }
    let Some(session) = current_session() else {
        return Ok(());
    };
    let parent = child.real_parent_pcb().ok_or(SystemError::ESRCH)?;
    // create() goes through kthreadd: the requester's affinity is not inherited.
    // The real wait/reap parent must remain runnable away from the holder CPU.
    ProcessManager::set_cpus_allowed(&parent, CpuMask::from_cpu(session.target))?;
    crate::sched::wait_cpu_placement(&parent);
    let mut registered = session.child.lock_irqsave();
    if registered.strong_count() != 0 {
        return Err(SystemError::EBUSY);
    }
    *registered = Arc::downgrade(child);
    Ok(())
}

pub(crate) fn is_control_child() -> bool {
    current_session().is_some_and(|s| s.is_child(&ProcessManager::current_pcb()))
}

/// Commands are accepted only from the already registered, real UMH child.
pub(crate) fn control_write(buf: &[u8]) -> Result<usize, SystemError> {
    let session = current_session().ok_or(SystemError::ENOENT)?;
    let child = ProcessManager::current_pcb();
    if !session.is_child(&child) || session.cancelled.load(Ordering::Acquire) {
        return Err(SystemError::EPERM);
    }
    match buf {
        b"borrow_arm\n" => arm(&session, &child)?,
        b"borrow_start\n" => start(&session)?,
        _ => return Err(SystemError::EINVAL),
    }
    Ok(buf.len())
}

fn arm(session: &Arc<Session>, child: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
    if smp_get_processor_id() != session.source || session.armed.swap(true, Ordering::AcqRel) {
        return Err(SystemError::EINVAL);
    }
    let mm = child.basic().user_vm().ok_or(SystemError::EFAULT)?;
    if mm.user_count() != 1 {
        return Err(SystemError::EBUSY);
    }
    *session.mm.lock_irqsave() = Arc::downgrade(&mm);
    drop(mm);
    let worker_session = session.clone();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            hold_natural_root(&worker_session);
            0
        }),
        (),
    ));
    let worker =
        KernelThreadMechanism::create_on_cpu(closure, "null-mm-holder".to_string(), session.source)
            .ok_or(SystemError::ENOMEM)?;
    *session.worker.lock_irqsave() = Some(worker);
    Ok(())
}

fn start(session: &Arc<Session>) -> Result<(), SystemError> {
    if smp_get_processor_id() != session.target
        || !session.armed.load(Ordering::Acquire)
        || session.started.swap(true, Ordering::AcqRel)
    {
        return Err(SystemError::EINVAL);
    }
    let worker = session
        .worker
        .lock_irqsave()
        .clone()
        .ok_or(SystemError::ESRCH)?;
    ProcessManager::wakeup(&worker)?;
    if !session
        .ready
        .wait_for_completion_timeout(BUDGET as i64)
        .is_ok_and(|n| n > 0)
        || !session.admitted.load(Ordering::Acquire)
    {
        session.cancelled.store(true, Ordering::Release);
        return Err(SystemError::EIO);
    }
    Ok(())
}

fn hold_natural_root(session: &Session) {
    if session.cancelled.load(Ordering::Acquire) {
        session.ready.complete_all();
        return;
    }
    // Resolve the test Weak before the guard, but never install this mm.
    let Some(mm) = session.mm.lock_irqsave().upgrade() else {
        session.ready.complete_all();
        return;
    };
    let task = ProcessManager::current_pcb();
    let no_user_mm = task.basic().user_vm().is_none();
    let guard = PreemptGuard::new();
    let admitted = {
        let _irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let cpu = smp_get_processor_id();
        let loaded = super::tlb_state_force(cpu);
        let ok = cpu == session.source
            && no_user_mm
            && loaded.loaded_is(&mm)
            && unsafe { MMArch::table(PageTableKind::User) } == mm.table_paddr()
            && mm.active_cpus.lock().get(cpu).unwrap_or(false);
        session
            .initial_gen
            .store(mm.tlb_gen.load(Ordering::SeqCst), Ordering::Relaxed);
        ok
    };
    if !admitted || !CurrentIrqArch::is_irq_enabled() {
        drop(guard);
        drop(mm);
        session.ready.complete_all();
        return;
    }
    session.admitted.store(true, Ordering::Release);
    session.ready.complete_all();
    let end = clock().saturating_add(BUDGET);
    for i in 0..MAX_SPINS {
        if session.cancelled.load(Ordering::Acquire) || session.reaped.load(Ordering::Acquire) {
            break;
        }
        if i & 1023 == 0 && clock() >= end {
            break;
        }
        spin_loop();
    }
    let passed = session.reaped.load(Ordering::Acquire)
        && session.reaped_ok.load(Ordering::Acquire)
        && !session.cancelled.load(Ordering::Acquire)
        && {
            let _irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
            let cpu = smp_get_processor_id();
            let loaded = super::tlb_state_force(cpu);
            let final_gen = mm.tlb_gen.load(Ordering::SeqCst);
            let loaded_gen = loaded.loaded_tlb_gen();
            session.final_gen.store(final_gen, Ordering::Relaxed);
            session.loaded_gen.store(loaded_gen, Ordering::Relaxed);
            let no_users = mm.user_count() == 0;
            let torn_down = mm.is_torn_down();
            let root_held = loaded.loaded_is(&mm)
                && unsafe { MMArch::table(PageTableKind::User) } == mm.table_paddr()
                && mm.active_cpus.lock().get(cpu).unwrap_or(false);
            session.no_users.store(no_users, Ordering::Relaxed);
            session.torn_down.store(torn_down, Ordering::Relaxed);
            session.root_held.store(root_held, Ordering::Relaxed);
            no_users
                && torn_down
                && root_held
                && final_gen > session.initial_gen.load(Ordering::Relaxed)
                && loaded_gen >= final_gen
        };
    // In a failed root-invariant case this might be the last test pin. Never
    // run its potentially faultable final destruction while non-preemptible.
    // stop/join completes only after this temporary is gone.
    drop(guard);
    drop(mm);
    session.passed.store(passed, Ordering::Release);
}

fn run_pair(source: ProcessorId, target: ProcessorId) -> Result<String, SystemError> {
    let session = Arc::new(Session::new(source, target));
    {
        let mut active = SESSION.lock_irqsave();
        if active.is_some() {
            return Err(SystemError::EBUSY);
        }
        *active = Some(session.clone());
    }
    let callback_session = session.clone();
    let launched = UserModeHelper::start_with_context(
        HELPER.into(),
        vec![
            CString::new(HELPER).unwrap(),
            CString::new("--borrow-child").unwrap(),
            CString::new(format!("{}", source.data())).unwrap(),
            CString::new(format!("{}", target.data())).unwrap(),
        ],
        Vec::new(),
        None,
        Some(Box::new(move |status| {
            callback_session
                .reaped_ok
                .store(matches!(status, Ok(0)), Ordering::Relaxed);
            if !matches!(status, Ok(0)) {
                callback_session.cancelled.store(true, Ordering::Release);
            }
            callback_session.reaped.store(true, Ordering::Release);
            callback_session.child_done.complete_all();
        })),
    );
    let helper_ok = match launched {
        Ok(helper) => {
            let status = helper.wait();
            if !matches!(status, Ok(0)) {
                session.cancelled.store(true, Ordering::Release);
            }
            // A killable requester wait must not detach callbacks still in flight.
            let done = session.child_done.wait_for_completion().is_ok();
            done && matches!(status, Ok(0))
        }
        Err(_) => {
            session.cancelled.store(true, Ordering::Release);
            false
        }
    };
    if !helper_ok {
        session.cancelled.store(true, Ordering::Release);
    }
    let worker = session.worker.lock_irqsave().take();
    let joined = worker
        .as_ref()
        .is_some_and(|worker| matches!(KernelThreadMechanism::stop(worker), Ok(0)));
    drop(worker);
    let weak = session.mm.lock_irqsave().clone();
    let retired_slot = SESSION.lock_irqsave().take();
    drop(retired_slot);
    let end = clock().saturating_add(BUDGET);
    // Joining our holder does not join other naturally scheduled borrowers.
    // Pin only the storage while observing their real active-bit retirement;
    // this neither adds a user nor prevents a CPU from switching away.
    let mut inactive = true;
    if let Some(mm) = weak.upgrade() {
        inactive = false;
        while clock() < end {
            if mm.active_cpus.lock().is_empty() {
                inactive = true;
                break;
            }
            crate::sched::cond_resched();
        }
        drop(mm);
    }
    let mut retired = false;
    while clock() < end {
        if weak.upgrade().is_none() {
            retired = true;
            break;
        }
        crate::sched::cond_resched();
    }
    let ok = helper_ok && joined && inactive && retired && session.passed.load(Ordering::Acquire);
    Ok(format!("borrow_{}_{}={} admitted={} helper_reaped={} worker_joined={} inactive={} retired={} no_users={} torn_down={} root_held={} initial_gen={} final_gen={} loaded_gen={}\n",
        source.data(), target.data(), if ok { "ok" } else { "fail" },
        session.admitted.load(Ordering::Acquire), helper_ok, joined, inactive, retired,
        session.no_users.load(Ordering::Relaxed), session.torn_down.load(Ordering::Relaxed), session.root_held.load(Ordering::Relaxed),
        session.initial_gen.load(Ordering::Relaxed), session.final_gen.load(Ordering::Relaxed), session.loaded_gen.load(Ordering::Relaxed)))
}

/// Runs once per fixed CPU pair; a wrong natural root is a failure, not a retry.
pub(crate) fn report() -> Result<String, SystemError> {
    let _serial = SERIAL.lock();
    let online = |n| smp_cpu_manager().is_online_cpu(ProcessorId::new(n));
    if !online(0) || !online(1) {
        return Ok("borrow_lifetime=not_applicable requires_two_cpus\n".into());
    }
    let caller = ProcessManager::current_pcb();
    let saved = caller.sched_info().cpus_allowed();
    let mut report = String::new();
    let result = (|| {
        for (source, target) in [(0, 1), (2, 3)] {
            if !online(source) || !online(target) {
                continue;
            }
            let target = ProcessorId::new(target);
            ProcessManager::set_cpus_allowed(&caller, CpuMask::from_cpu(target))?;
            crate::sched::wait_cpu_placement(&caller);
            report.push_str(&run_pair(ProcessorId::new(source), target)?);
        }
        Ok(report)
    })();
    let restored = ProcessManager::set_cpus_allowed(&caller, saved);
    crate::sched::wait_cpu_placement(&caller);
    restored?;
    result
}
