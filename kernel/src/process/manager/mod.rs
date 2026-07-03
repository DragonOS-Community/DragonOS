use core::{
    hint::spin_loop,
    intrinsics::unlikely,
    sync::atomic::{compiler_fence, fence, AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use alloc::{sync::Arc, vec::Vec};
use hashbrown::HashMap;
use log::{debug, error, info};
use system_error::SystemError;

use crate::{
    libs::{
        mutex::{Mutex, MutexGuard},
        spinlock::SpinLock,
    },
    mm::{
        percpu::{PerCpu, PerCpuVar},
        set_IDLE_PROCESS_ADDRESS_SPACE,
        ucontext::AddressSpace,
    },
    process::{ProcessControlBlock, RawPid},
    sched::{cpu_rq, enqueue_task_on_cpu, WakeupFlags},
    smp::{core::smp_get_processor_id, cpu::ProcessorId, kick_cpu},
    syscall::user_access::write_one_to_user_protected,
};

mod exit;
mod sched;

#[derive(Debug)]
pub struct ProcessManager;

static ALL_PROCESS: SpinLock<Option<HashMap<RawPid, Arc<ProcessControlBlock>>>> =
    SpinLock::new(None);
pub(super) static PTRACE_RELATION_LOCK: SpinLock<()> = SpinLock::new(());
static NR_VISIBLE_THREADS: AtomicUsize = AtomicUsize::new(0);
static TOTAL_FORKS: AtomicU64 = AtomicU64::new(0);
static TOTAL_CONTEXT_SWITCHES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn all_process() -> &'static SpinLock<Option<HashMap<RawPid, Arc<ProcessControlBlock>>>>
{
    &ALL_PROCESS
}

#[inline]
pub(crate) fn inc_visible_thread_count() {
    NR_VISIBLE_THREADS.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub(crate) fn dec_visible_thread_count() {
    NR_VISIBLE_THREADS.fetch_sub(1, Ordering::Relaxed);
}

#[inline]
pub(crate) fn account_successful_fork() {
    TOTAL_FORKS.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn nr_threads() -> u32 {
    NR_VISIBLE_THREADS.load(Ordering::Relaxed) as u32
}

#[inline]
pub fn total_forks() -> u64 {
    TOTAL_FORKS.load(Ordering::Relaxed)
}

#[inline]
pub(crate) fn account_context_switch() {
    TOTAL_CONTEXT_SWITCHES.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn nr_context_switches() -> u64 {
    TOTAL_CONTEXT_SWITCHES.load(Ordering::Relaxed)
}

fn exchange_raw_pids_locked(
    map: &mut HashMap<RawPid, Arc<ProcessControlBlock>>,
    left: &Arc<ProcessControlBlock>,
    right: &Arc<ProcessControlBlock>,
) -> Result<(), SystemError> {
    let left_pid = left.raw_pid();
    let right_pid = right.raw_pid();
    if left_pid == right_pid {
        return Err(SystemError::EINVAL);
    }
    let left_entry = map.remove(&left_pid).ok_or(SystemError::ESRCH)?;
    let right_entry = map.remove(&right_pid).ok_or(SystemError::ESRCH)?;

    unsafe {
        left.force_set_raw_pid(right_pid);
        right.force_set_raw_pid(left_pid);
    }

    map.insert(right_pid, left_entry);
    map.insert(left_pid, right_entry);
    Ok(())
}

pub static mut PROCESS_SWITCH_RESULT: Option<PerCpuVar<SwitchResult>> = None;

/// A global flag that is set once when the process manager has finished
/// initializing.
pub(super) static mut __PROCESS_MANAGEMENT_INIT_DONE: bool = false;

/// Serializes `/proc/[pid]/oom_score_adj` writes with `CLONE_VM` inheritance.
///
/// This mirrors Linux's `oom_adj_mutex` responsibility: a process sharing an
/// mm with another thread group must not miss a concurrent oom_score_adj update
/// while it is becoming visible.
static OOM_SCORE_ADJ_LOCK: Mutex<()> = Mutex::new(());

pub struct SwitchResult {
    pub prev_pcb: Option<Arc<ProcessControlBlock>>,
    pub next_pcb: Option<Arc<ProcessControlBlock>>,
    pub migrate_prev_to: Option<ProcessorId>,
}

impl SwitchResult {
    pub fn new() -> Self {
        Self {
            prev_pcb: None,
            next_pcb: None,
            migrate_prev_to: None,
        }
    }
}

impl ProcessManager {
    pub fn lock_oom_score_adj() -> MutexGuard<'static, ()> {
        OOM_SCORE_ADJ_LOCK.lock()
    }

    fn mm_has_user_tasks(mm: &Arc<AddressSpace>) -> bool {
        ProcessManager::get_all_processes()
            .into_iter()
            .filter_map(ProcessManager::find)
            .any(|task| {
                task.basic()
                    .user_vm()
                    .is_some_and(|task_mm| task_mm.id() == mm.id() || Arc::ptr_eq(&task_mm, mm))
            })
    }

    pub fn is_current(pcb: &Arc<ProcessControlBlock>) -> bool {
        Arc::ptr_eq(pcb, &Self::current_pcb())
    }

    pub fn thread_group_leader_of(pcb: &Arc<ProcessControlBlock>) -> Arc<ProcessControlBlock> {
        pcb.threads_read_irqsave()
            .group_leader()
            .unwrap_or_else(|| pcb.clone())
    }

    /// Iterate over all threads in the thread group that `pcb` belongs to.
    ///
    /// The thread group leader is visited first, followed by the other threads
    /// in the list maintained by the leader. Iteration stops early when the
    /// callback returns `false`.
    pub fn for_each_thread_in_group<F>(pcb: Arc<ProcessControlBlock>, mut func: F)
    where
        F: FnMut(Arc<ProcessControlBlock>) -> bool,
    {
        let thread_group_leader = Self::thread_group_leader_of(&pcb);
        if !func(thread_group_leader.clone()) {
            return;
        }

        let group_tasks = thread_group_leader
            .threads_read_irqsave()
            .group_tasks_clone();
        for weak in group_tasks {
            let Some(task) = weak.upgrade() else {
                continue;
            };
            if Arc::ptr_eq(&task, &thread_group_leader) {
                continue;
            }
            if !func(task) {
                break;
            }
        }
    }

    pub fn thread_group_tasks_snapshot(
        pcb: Arc<ProcessControlBlock>,
    ) -> Vec<Arc<ProcessControlBlock>> {
        let leader = Self::thread_group_leader_of(&pcb);
        let mut tasks = Vec::new();
        tasks.push(leader.clone());

        let group_tasks = leader.threads_read_irqsave().group_tasks_clone();
        for weak in group_tasks {
            let Some(task) = weak.upgrade() else {
                continue;
            };
            if tasks.iter().any(|existing| Arc::ptr_eq(existing, &task)) {
                continue;
            }
            tasks.push(task);
        }

        tasks
    }

    #[inline(never)]
    pub(super) fn init() {
        static INIT_FLAG: AtomicBool = AtomicBool::new(false);
        if INIT_FLAG
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            panic!("ProcessManager has been initialized!");
        }

        unsafe {
            compiler_fence(Ordering::SeqCst);
            debug!("To create address space for INIT process.");
            // test_buddy();
            set_IDLE_PROCESS_ADDRESS_SPACE(
                AddressSpace::new(true).expect("Failed to create address space for INIT process."),
            );
            debug!("INIT process address space created.");
            compiler_fence(Ordering::SeqCst);
        };

        ALL_PROCESS.lock_irqsave().replace(HashMap::new());
        Self::init_switch_result();
        Self::arch_init();
        debug!("process arch init done.");
        Self::init_idle();
        debug!("process idle init done.");

        unsafe { __PROCESS_MANAGEMENT_INIT_DONE = true };
        info!("Process Manager initialized.");
    }

    fn init_switch_result() {
        let mut switch_res_vec: Vec<SwitchResult> = Vec::new();
        for _ in 0..PerCpu::MAX_CPU_NUM {
            switch_res_vec.push(SwitchResult::new());
        }
        unsafe {
            PROCESS_SWITCH_RESULT = Some(PerCpuVar::new(switch_res_vec).unwrap());
        }
    }

    /// Returns whether the process manager has finished initializing.
    #[allow(dead_code)]
    pub fn initialized() -> bool {
        unsafe { __PROCESS_MANAGEMENT_INIT_DONE }
    }

    /// Returns the PCB of the current process.
    pub fn current_pcb() -> Arc<ProcessControlBlock> {
        if unlikely(unsafe { !__PROCESS_MANAGEMENT_INIT_DONE }) {
            error!("unsafe__PROCESS_MANAGEMENT_INIT_DONE == false");
            loop {
                spin_loop();
            }
        }
        return ProcessControlBlock::arch_current_pcb();
    }

    /// Returns the PID of the current process.
    ///
    /// Returns 0 if the process manager has not yet finished initializing.
    pub fn current_pid() -> RawPid {
        if unlikely(unsafe { !__PROCESS_MANAGEMENT_INIT_DONE }) {
            return RawPid(0);
        }

        return ProcessManager::current_pcb().raw_pid();
    }

    /// Look up a process's PCB by PID.
    ///
    /// ## Parameters
    ///
    /// - `pid`: The PID of the process.
    ///
    /// ## Returns
    ///
    /// The PCB of the process if found, otherwise `None`.
    pub fn find(pid: RawPid) -> Option<Arc<ProcessControlBlock>> {
        return ALL_PROCESS.lock_irqsave().as_ref()?.get(&pid).cloned();
    }

    /// Add a process's PCB to the system.
    ///
    /// ## Parameters
    ///
    /// - `pcb`: The PCB of the process.
    pub fn add_pcb(pcb: Arc<ProcessControlBlock>) {
        ALL_PROCESS
            .lock_irqsave()
            .as_mut()
            .unwrap()
            .insert(pcb.raw_pid(), pcb.clone());
    }

    pub(crate) fn exchange_tid_and_raw_pids(
        left: &Arc<ProcessControlBlock>,
        right: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        let _cgroup_guard = crate::cgroup::cgroup_accounting_lock().lock();
        let mut all_proc = all_process().lock_irqsave();
        let map = all_proc.as_mut().ok_or(SystemError::EINVAL)?;
        let left_old_pid = left.raw_pid();
        let right_old_pid = right.raw_pid();
        if left_old_pid == right_old_pid {
            return Err(SystemError::EINVAL);
        }
        if !map.contains_key(&left_old_pid) || !map.contains_key(&right_old_pid) {
            return Err(SystemError::ESRCH);
        }

        let left_cgroup = left.task_cgroup_node();
        let right_cgroup = right.task_cgroup_node();
        let left_alive = !left.is_exited();
        let right_alive = !right.is_exited();

        left.exchange_tid_with(right)?;
        exchange_raw_pids_locked(map, left, right)?;

        // cgroup.procs only shows tasks that are still alive; when exec
        // detaches threads and swaps pids, only rename the visible members.
        if Arc::ptr_eq(&left_cgroup, &right_cgroup) && left_alive && right_alive {
            return Ok(());
        }
        if left_alive {
            left_cgroup.rename_task(left_old_pid, left.raw_pid());
        }
        if right_alive {
            right_cgroup.rename_task(right_old_pid, right.raw_pid());
        }

        Ok(())
    }

    /// ### Returns the PIDs of all processes.
    pub fn get_all_processes() -> Vec<RawPid> {
        let mut pids = Vec::new();
        for (pid, _) in ALL_PROCESS.lock_irqsave().as_ref().unwrap().iter() {
            pids.push(*pid);
        }
        pids
    }

    /// Hook function called after a context switch has completed.
    pub(super) unsafe fn switch_finish_hook() {
        // debug!("switch_finish_hook");
        let prev_pcb = PROCESS_SWITCH_RESULT
            .as_mut()
            .unwrap()
            .get_mut()
            .prev_pcb
            .take()
            .expect("prev_pcb is None");
        let next_pcb = PROCESS_SWITCH_RESULT
            .as_mut()
            .unwrap()
            .get_mut()
            .next_pcb
            .take()
            .expect("next_pcb is None");

        // SpinLockGuard::leak() was used before the context switch, so the locks
        // must be manually released here.
        fence(Ordering::SeqCst);

        prev_pcb.arch_info.force_unlock();
        fence(Ordering::SeqCst);

        next_pcb.arch_info.force_unlock();
        fence(Ordering::SeqCst);

        let migrate_prev_to = PROCESS_SWITCH_RESULT
            .as_mut()
            .unwrap()
            .get_mut()
            .migrate_prev_to
            .take();

        if let Some(dest_cpu) = migrate_prev_to {
            debug_assert!(!Arc::ptr_eq(&prev_pcb, &next_pcb));
            prev_pcb.sched_info().set_on_cpu(None);
            enqueue_task_on_cpu(&prev_pcb, dest_cpu, WakeupFlags::WF_MIGRATED, false);
        }

        let set_child_tid = next_pcb.thread.write_irqsave().set_child_tid.take();
        if let Some(addr) = set_child_tid {
            // Align with Linux schedule_tail semantics: best-effort write of tid
            // when the child task runs for the first time. Failure does not
            // prevent the thread from continuing.
            let child_tid = next_pcb.task_pid_vnr().data() as i32;
            let _ = unsafe { write_one_to_user_protected(addr, &child_tid) };
        }
    }

    /// If the target process is running on a remote CPU, force that CPU into
    /// kernel mode.
    ///
    /// ## Parameters
    ///
    /// - `pcb`: The PCB of the process.
    #[allow(dead_code)]
    pub fn kick(pcb: &Arc<ProcessControlBlock>) {
        ProcessManager::current_pcb().preempt_disable();
        let cpu_id = pcb.sched_info().on_cpu();

        if let Some(cpu_id) = cpu_id {
            let current_cpu_id = smp_get_processor_id();
            // DragonOS does not currently have Linux's lockless rq.current
            // convention as in `kick_process()`. The remote rq.current must be
            // read while holding the target rq lock.
            let should_kick = if cpu_id != current_cpu_id {
                let rq = cpu_rq(cpu_id.data() as usize);
                let (rq, _guard) = rq.self_lock();
                Arc::ptr_eq(&rq.current(), pcb)
            } else {
                false
            };

            // Do not kick the current CPU, as it is already running and cannot preempt itself.
            if should_kick {
                kick_cpu(cpu_id).expect("ProcessManager::kick(): Failed to kick cpu");
            }
        }

        ProcessManager::current_pcb().preempt_enable();
    }
}
