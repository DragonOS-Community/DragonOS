use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU8, Ordering};

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::cpu::current_cpu_id,
    libs::{
        cpumask::CpuMask,
        rwlock::RwLock,
        spinlock::{SpinLock, SpinLockGuard},
    },
    process::{ProcessControlBlock, ProcessState},
    sched::{cpu_is_online, fair::FairSchedEntity, prio::MAX_PRIO, OnRq, SchedPolicy},
    smp::cpu::{AtomicProcessorId, ProcessorId},
};

#[derive(Debug)]
pub struct ProcessSchedulerInfo {
    /// The CPU the current process is on.
    on_cpu: AtomicProcessorId,
    /// If the current process is waiting to be migrated to another CPU core
    /// (i.e. PF_NEED_MIGRATE is set in flags), this field stores the target
    /// processor core number.
    migrate_to: AtomicProcessorId,
    state_atomic: AtomicU32,
    pi_lock: SpinLock<PiProtected>,
    /// Scheduler priority of the process.
    // priority: SchedPriority,
    /// Virtual runtime of the current process.
    // virtual_runtime: AtomicIsize,
    /// Time slice managed by the real-time scheduler.
    // rt_time_slice: AtomicIsize,
    pub sched_stat: RwLock<SchedInfo>,
    /// Scheduling policy (protected by rq_lock / pi_lock).
    sched_policy: AtomicU8,
    /// CFS scheduling entity.
    pub sched_entity: Arc<FairSchedEntity>,
    pub on_rq: SpinLock<OnRq>,
    placement: SpinLock<NewTaskPlacement>,

    /// Protected by rq_lock.
    prio: AtomicI32,
    static_prio: AtomicI32,
    normal_prio: AtomicI32,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NewTaskPlacement {
    is_new_task: bool,
    target_cpu_hint: Option<ProcessorId>,
}

#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct SchedInfo {
    /// Number of times the task has run on a particular CPU.
    pub pcount: usize,
    /// Time the task has spent waiting on the run queue.
    pub run_delay: usize,
    /// Timestamp of when the task last ran on a CPU.
    pub last_arrival: u64,
    /// Timestamp of when the task was last added to the run queue.
    pub last_queued: u64,
}

/// Fields protected by pi_lock.
#[derive(Debug)]
pub struct PiProtected {
    pub cpus_allowed: CpuMask,
    pub nr_cpus_allowed: usize,
}

impl PiProtected {
    pub fn new(cpus_allowed: CpuMask) -> Self {
        let nr_cpus_allowed = cpus_allowed.iter_cpu().count();
        Self {
            cpus_allowed,
            nr_cpus_allowed,
        }
    }

    pub fn set_cpus_allowed(&mut self, new_mask: CpuMask) {
        self.cpus_allowed = new_mask;
        self.nr_cpus_allowed = self.cpus_allowed.iter_cpu().count();
    }
}

impl ProcessSchedulerInfo {
    fn default_cpus_allowed() -> CpuMask {
        if crate::smp::cpu::smp_cpu_manager_initialized() {
            return crate::smp::cpu::smp_cpu_manager().possible_cpus().clone();
        }

        // Process management initialization happens before SMP topology
        // initialization. Fall back to “runnable on the current boot CPU” here,
        // and converge to a more precise mask later during explicit
        // initialization (e.g. idle/per-cpu kthread).
        CpuMask::from_cpu(current_cpu_id())
    }

    #[inline(never)]
    pub fn new(on_cpu: Option<ProcessorId>) -> Self {
        let cpu_id = on_cpu.unwrap_or(ProcessorId::INVALID);
        let cpus_allowed = Self::default_cpus_allowed();
        return Self {
            on_cpu: AtomicProcessorId::new(cpu_id),
            migrate_to: AtomicProcessorId::new(ProcessorId::INVALID),
            state_atomic: AtomicU32::new(ProcessState::Blocked(false).to_u32()),
            pi_lock: SpinLock::new(PiProtected::new(cpus_allowed)),
            // virtual_runtime: AtomicIsize::new(0),
            // rt_time_slice: AtomicIsize::new(0),
            // priority: SchedPriority::new(100).unwrap(),
            sched_stat: RwLock::new(SchedInfo::default()),
            sched_policy: AtomicU8::new(SchedPolicy::CFS.to_u8()),
            sched_entity: FairSchedEntity::new(),
            on_rq: SpinLock::new(OnRq::None),
            placement: SpinLock::new(NewTaskPlacement::default()),
            prio: AtomicI32::new(MAX_PRIO - 20),
            static_prio: AtomicI32::new(MAX_PRIO - 20),
            normal_prio: AtomicI32::new(MAX_PRIO - 20),
        };
    }

    pub fn sched_entity(&self) -> Arc<FairSchedEntity> {
        return self.sched_entity.clone();
    }

    pub fn on_cpu(&self) -> Option<ProcessorId> {
        let on_cpu = self.on_cpu.load(Ordering::SeqCst);
        if on_cpu == ProcessorId::INVALID {
            return None;
        } else {
            return Some(on_cpu);
        }
    }

    pub fn set_on_cpu(&self, on_cpu: Option<ProcessorId>) {
        if let Some(cpu_id) = on_cpu {
            self.on_cpu.store(cpu_id, Ordering::SeqCst);
        } else {
            self.on_cpu.store(ProcessorId::INVALID, Ordering::SeqCst);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn placement_lock(&self) -> SpinLockGuard<'_, NewTaskPlacement> {
        self.placement.lock_irqsave()
    }

    pub fn mark_new_task(&self, target_cpu_hint: Option<ProcessorId>) {
        let mut guard = self.placement.lock_irqsave();
        guard.is_new_task = true;
        guard.target_cpu_hint = target_cpu_hint;
    }

    /// # Parameters
    /// - `allowed`: The caller must pre-read cpus_allowed under pi_lock
    ///   protection and pass it in to avoid re-entering pi_lock.
    pub fn consume_new_task_target_cpu(
        &self,
        current_cpu: ProcessorId,
        allowed: CpuMask,
        default_selector: impl FnOnce(&CpuMask) -> Option<ProcessorId>,
    ) -> Result<ProcessorId, SystemError> {
        let mut placement = self.placement.lock_irqsave();
        if !placement.is_new_task {
            return Err(SystemError::EINVAL);
        }
        let selected_cpu = default_selector(&allowed).filter(|&cpu| {
            allowed.get(cpu).unwrap_or(false)
                && (!crate::smp::cpu::smp_cpu_manager_initialized() || cpu_is_online(cpu))
        });
        let target_cpu = if let Some(target_cpu) = placement.target_cpu_hint {
            if allowed.get(target_cpu).unwrap_or(false)
                && (!crate::smp::cpu::smp_cpu_manager_initialized() || cpu_is_online(target_cpu))
            {
                target_cpu
            } else if let Some(selected_cpu) = selected_cpu {
                selected_cpu
            } else if allowed.get(current_cpu).unwrap_or(false)
                && (!crate::smp::cpu::smp_cpu_manager_initialized() || cpu_is_online(current_cpu))
            {
                current_cpu
            } else {
                allowed
                    .iter_cpu()
                    .find(|&cpu| {
                        !crate::smp::cpu::smp_cpu_manager_initialized() || cpu_is_online(cpu)
                    })
                    .ok_or(SystemError::EINVAL)?
            }
        } else if let Some(selected_cpu) = selected_cpu {
            selected_cpu
        } else if allowed.get(current_cpu).unwrap_or(false)
            && (!crate::smp::cpu::smp_cpu_manager_initialized() || cpu_is_online(current_cpu))
        {
            current_cpu
        } else {
            allowed
                .iter_cpu()
                .find(|&cpu| !crate::smp::cpu::smp_cpu_manager_initialized() || cpu_is_online(cpu))
                .ok_or(SystemError::EINVAL)?
        };

        placement.is_new_task = false;
        placement.target_cpu_hint = None;
        Ok(target_cpu)
    }

    pub fn is_new_task(&self) -> bool {
        self.placement.lock_irqsave().is_new_task
    }

    pub fn migrate_to(&self) -> Option<ProcessorId> {
        let migrate_to = self.migrate_to.load(Ordering::SeqCst);
        if migrate_to == ProcessorId::INVALID {
            None
        } else {
            Some(migrate_to)
        }
    }

    pub fn set_migrate_to(&self, migrate_to: Option<ProcessorId>) {
        self.migrate_to
            .store(migrate_to.unwrap_or(ProcessorId::INVALID), Ordering::SeqCst);
    }

    pub fn state(&self) -> ProcessState {
        ProcessState::from_u32(self.state_atomic.load(Ordering::Acquire))
    }

    pub fn set_state(&self, state: ProcessState) {
        self.state_atomic.store(state.to_u32(), Ordering::Release);
    }

    /// Acquire pi_lock (SpinLock), disabling interrupts and preemption.
    /// Lock ordering: pi_lock may nest rq_lock (pi_lock → rq_lock); the reverse
    /// is prohibited.
    pub fn pi_lock_irqsave(&self) -> SpinLockGuard<'_, PiProtected> {
        self.pi_lock.lock_irqsave()
    }

    // pub fn virtual_runtime(&self) -> isize {
    //     return self.virtual_runtime.load(Ordering::SeqCst);
    // }

    // pub fn set_virtual_runtime(&self, virtual_runtime: isize) {
    //     self.virtual_runtime
    //         .store(virtual_runtime, Ordering::SeqCst);
    // }
    // pub fn increase_virtual_runtime(&self, delta: isize) {
    //     self.virtual_runtime.fetch_add(delta, Ordering::SeqCst);
    // }

    // pub fn rt_time_slice(&self) -> isize {
    //     return self.rt_time_slice.load(Ordering::SeqCst);
    // }

    // pub fn set_rt_time_slice(&self, rt_time_slice: isize) {
    //     self.rt_time_slice.store(rt_time_slice, Ordering::SeqCst);
    // }

    // pub fn increase_rt_time_slice(&self, delta: isize) {
    //     self.rt_time_slice.fetch_add(delta, Ordering::SeqCst);
    // }

    /// Read the scheduling policy.
    #[inline]
    pub fn policy(&self) -> SchedPolicy {
        SchedPolicy::from_u8(self.sched_policy.load(Ordering::Relaxed))
    }

    /// Set the scheduling policy.
    #[inline]
    pub fn set_policy(&self, policy: SchedPolicy) {
        self.sched_policy.store(policy.to_u8(), Ordering::Relaxed);
    }

    /// Read the dynamic priority.
    #[inline]
    pub fn prio(&self) -> i32 {
        self.prio.load(Ordering::Relaxed)
    }

    /// Set the dynamic priority.
    #[inline]
    pub fn set_prio(&self, val: i32) {
        self.prio.store(val, Ordering::Relaxed);
    }

    /// Read the static priority.
    #[inline]
    pub fn static_prio(&self) -> i32 {
        self.static_prio.load(Ordering::Relaxed)
    }

    /// Set the static priority.
    #[inline]
    pub fn set_static_prio(&self, val: i32) {
        self.static_prio.store(val, Ordering::Relaxed);
    }

    /// Read the normal priority.
    #[inline]
    pub fn normal_prio(&self) -> i32 {
        self.normal_prio.load(Ordering::Relaxed)
    }

    /// Set the normal priority.
    #[inline]
    pub fn set_normal_prio(&self, val: i32) {
        self.normal_prio.store(val, Ordering::Relaxed);
    }

    /// Set all priorities at once.
    #[inline]
    pub fn set_prio_all(&self, prio: i32, static_prio: i32, normal_prio: i32) {
        self.prio.store(prio, Ordering::Relaxed);
        self.static_prio.store(static_prio, Ordering::Relaxed);
        self.normal_prio.store(normal_prio, Ordering::Relaxed);
    }

    pub fn cpus_allowed(&self) -> CpuMask {
        self.pi_lock.lock_irqsave().cpus_allowed.clone()
    }

    pub fn nr_cpus_allowed(&self) -> usize {
        self.pi_lock.lock_irqsave().nr_cpus_allowed
    }

    pub fn set_cpus_allowed(&self, cpus_allowed: CpuMask) {
        self.pi_lock.lock_irqsave().set_cpus_allowed(cpus_allowed);
    }
}

impl ProcessControlBlock {
    #[inline]
    pub(crate) fn debug_assert_fork_cpu_binding(&self) {
        if cfg!(debug_assertions) {
            let Some(on_cpu) = self.sched_info().on_cpu() else {
                return;
            };

            debug_assert_eq!(
                self.sched_info().sched_entity().cfs_rq().rq().cpu(),
                on_cpu,
                "fork target cpu and SE bound rq must stay consistent"
            );
        }
    }
}
