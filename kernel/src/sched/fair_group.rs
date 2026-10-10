//! CFS task-group ownership and scheduler-side attachment.
//!
//! Cgroup topology/configuration is serialized by cgroup::UPDATE_LOCK; this
//! module changes CPU-local scheduling state only while holding the owner rq.
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use system_error::SystemError;

use crate::mm::percpu::PerCpu;
use crate::process::ProcessControlBlock;
use crate::smp::cpu::ProcessorId;

use super::fair::{CfsRunQueue, CompletelyFairScheduler, FairSchedEntity};
use super::{
    cpu_is_online, cpu_rq, DequeueFlag, EnqueueFlag, LoadWeight, OnRq, SchedClass, TaskGroup,
};

impl TaskGroup {
    /// Allocate an unpublished non-root group. Parent None denotes root.
    pub(crate) fn new(parent: Option<Arc<TaskGroup>>) -> Arc<Self> {
        let group = Arc::new_cyclic(|weak| {
            let mut entities = Vec::with_capacity(PerCpu::MAX_CPU_NUM as usize);
            let mut cfs = Vec::with_capacity(PerCpu::MAX_CPU_NUM as usize);
            for cpu in 0..PerCpu::MAX_CPU_NUM as usize {
                let rq = cpu_rq(cpu);
                let leaf = Arc::new(CfsRunQueue::new());
                leaf.force_mut().set_rq(Arc::downgrade(&rq));
                leaf.force_mut().set_task_group(weak.clone());
                let entity = FairSchedEntity::new();
                let parent_entity = parent.as_ref().map(|tg| tg.entities[cpu].clone());
                let parent_cfs = parent
                    .as_ref()
                    .map_or_else(|| rq.cfs_rq(), |tg| tg.cfs[cpu].clone());
                entity
                    .force_mut()
                    .init_group(leaf.clone(), &parent_cfs, parent_entity.as_ref());
                entities.push(entity);
                cfs.push(leaf);
            }
            Self {
                entities,
                cfs,
                parent,
                shares: AtomicU64::new(LoadWeight::NICE_0_LOAD),
                load_avg: AtomicU64::new(0),
                idle: AtomicBool::new(false),
                children: core::array::from_fn(
                    |_| crate::libs::spinlock::SpinLock::new(Vec::new()),
                ),
                bandwidth: super::cfs_bandwidth::CfsBandwidth::new(weak.clone()),
                retired: AtomicBool::new(false),
            }
        });
        if let Some(parent) = &group.parent {
            for cpu in 0..PerCpu::MAX_CPU_NUM as usize {
                let binding = cpu_rq(cpu);
                let (rq, _guard) = binding.self_lock();
                let leaf = group.cfs[cpu].force_mut();
                leaf.throttled_count = parent.cfs[cpu].throttled_count;
                if leaf.throttled_count != 0 {
                    leaf.throttled_clock_pelt = rq.rq_clock_pelt();
                }
                parent.children[cpu]
                    .lock_irqsave()
                    .push(Arc::downgrade(&group));
            }
        }
        group
    }

    pub(crate) fn cfs_rq(&self, cpu: ProcessorId) -> Arc<CfsRunQueue> {
        self.cfs[cpu.data() as usize].clone()
    }

    pub(crate) fn entity(&self, cpu: ProcessorId) -> Arc<FairSchedEntity> {
        self.entities[cpu.data() as usize].clone()
    }

    pub(crate) fn parent(&self) -> Option<Arc<TaskGroup>> {
        self.parent.clone()
    }

    pub(crate) fn shares(&self) -> u64 {
        self.shares.load(Ordering::Relaxed)
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.idle.load(Ordering::Relaxed)
    }

    /// Caller serializes configuration updates. rq locks protect reweighting.
    pub(crate) fn set_shares(&self, shares: u64) -> Result<(), SystemError> {
        if self.is_idle() {
            return Err(SystemError::EINVAL);
        }
        self.apply_shares(shares);
        Ok(())
    }

    fn apply_shares(&self, shares: u64) {
        // Linux MIN_SHARES/MAX_SHARES, in internal scaled units.
        self.shares.store(
            shares.clamp(LoadWeight::scale_load(2), LoadWeight::scale_load(262144)),
            Ordering::Relaxed,
        );
        for cpu in 0..PerCpu::MAX_CPU_NUM as usize {
            if !cpu_is_online(ProcessorId::new(cpu as u32)) {
                continue;
            }
            let binding = cpu_rq(cpu);
            let (rq, _guard) = binding.self_lock();
            rq.update_rq_clock();
            let mut se = self.entities[cpu].clone();
            FairSchedEntity::for_each_in_group(&mut se, |se| {
                se.cfs_rq().force_mut().refresh_group_entity(&se);
                (true, true)
            });
            rq.resched_current();
        }
    }

    pub(crate) fn set_idle(&self, idle: bool) {
        if self.idle.swap(idle, Ordering::Relaxed) == idle {
            return;
        }
        for cpu in 0..PerCpu::MAX_CPU_NUM as usize {
            if !cpu_is_online(ProcessorId::new(cpu as u32)) {
                continue;
            }
            let binding = cpu_rq(cpu);
            let (rq, _guard) = binding.self_lock();
            rq.update_rq_clock();
            self.cfs[cpu]
                .force_mut()
                .set_group_idle(&self.entities[cpu], idle);
            rq.resched_current();
        }
        // Linux intentionally resets shares when leaving idle mode.
        self.apply_shares(if idle {
            LoadWeight::scale_load(3)
        } else {
            LoadWeight::NICE_0_LOAD
        });
    }

    /// All member tasks (sleeping included) must have left before retirement.
    pub(crate) fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        self.bandwidth.retire();
        for cpu in 0..PerCpu::MAX_CPU_NUM as usize {
            let binding = cpu_rq(cpu);
            let (rq, _guard) = binding.self_lock();
            rq.update_rq_clock();
            let leaf = self.cfs[cpu].force_mut();
            leaf.runtime_enabled = false;
            leaf.runtime_remaining = 0;
            if leaf.throttled {
                leaf.unthrottle_bandwidth();
            }
            leaf.retire_group(&self.entities[cpu]);
        }
    }

    /// Rq lock protects every queue touched; release registry lock before f.
    pub(crate) fn for_each_descendant(
        self: &Arc<Self>,
        cpu: ProcessorId,
        f: &mut impl FnMut(&Arc<TaskGroup>),
    ) {
        let mut stack = alloc::vec![self.clone()];
        while let Some(group) = stack.pop() {
            f(&group);
            stack.extend(
                group.children[cpu.data() as usize]
                    .lock_irqsave()
                    .iter()
                    .filter_map(|child| child.upgrade()),
            );
        }
    }
}

impl Drop for TaskGroup {
    fn drop(&mut self) {
        // Retired exit tails still participate in ancestor PELT/throttle
        // propagation. Remove the weak topology entry only after the last
        // task/child reference is gone; never acquire an rq from Drop.
        if let Some(parent) = &self.parent {
            for children in &parent.children {
                children
                    .lock_irqsave()
                    .retain(|child| !core::ptr::eq(child.as_ptr(), self));
            }
        }
    }
}

/// Bind a detached/new entity to its effective group and target CPU.
/// The task is unpublished, or its pi lock plus appropriate rq protects it.
pub(crate) fn bind_task_group_cpu(task: &Arc<ProcessControlBlock>, cpu: ProcessorId) {
    let entity = task.sched_info().sched_entity();
    let group = task.sched_info().cpu_group();
    let (leaf, parent) = match group {
        Some(group) => (group.cfs_rq(cpu), Some(group.entity(cpu))),
        None => (cpu_rq(cpu.data() as usize).cfs_rq(), None),
    };
    entity.force_mut().bind_task(&leaf, parent.as_ref());
}

/// Move a task between CPU css instances, preserving its lag and PELT.
/// Caller holds the cgroup topology transaction; this acquires pi -> rq.
pub(crate) fn attach_cpu_group(task: &Arc<ProcessControlBlock>, group: Option<Arc<TaskGroup>>) {
    let accounting = task.task_cgroup_node().cpu_accounting();
    loop {
        let _pi = task.sched_info().pi_lock_irqsave();
        let old = task.sched_info().cpu_group();
        let unchanged = match (&old, &group) {
            (None, None) => true,
            (Some(old), Some(new)) => Arc::ptr_eq(old, new),
            _ => false,
        };
        let Some(cpu) = task.sched_info().on_cpu() else {
            task.sched_info().set_cpu_group_locked(group);
            task.replace_cpu_accounting_locked(accounting);
            return;
        };
        let binding = cpu_rq(cpu.data() as usize);
        let (rq, _guard) = binding.self_lock();
        let migrating = *task.sched_info().on_rq.lock_irqsave() == OnRq::Migrating;
        if migrating || task.sched_info().on_cpu() != Some(cpu) {
            // The switch tail owns the detached entity while Migrating. It needs
            // pi_lock to publish destination placement, so release both locks
            // before waiting; never attach PELT to the obsolete source rq.
            drop(_guard);
            drop(_pi);
            if migrating {
                super::wait_cpu_placement(task);
            }
            continue;
        }
        rq.update_rq_clock();
        let fair = task.sched_info().sched_class() == SchedClass::Fair;
        let queued = *task.sched_info().on_rq.lock_irqsave() == OnRq::Queued;
        let current = Arc::ptr_eq(&rq.current(), task);
        if current && task.sched_info().sched_class() == SchedClass::Realtime {
            super::realtime::RealtimeScheduler::update_bandwidth(rq, SchedClass::Realtime);
        }
        if unchanged {
            if current && fair {
                CompletelyFairScheduler::update_current_chain(task);
            }
            task.replace_cpu_accounting_locked(accounting);
            return;
        }
        if fair {
            if queued {
                CompletelyFairScheduler::dequeue(
                    rq,
                    task.clone(),
                    DequeueFlag::DEQUEUE_SAVE | DequeueFlag::DEQUEUE_MOVE,
                );
            }
            if current {
                CompletelyFairScheduler::put_prev_task(rq, task.clone());
            }
            CompletelyFairScheduler::switched_from_fair(rq, task);
            task.sched_info()
                .sched_entity()
                .force_mut()
                .avg
                .last_update_time = 0;
        }
        task.replace_cpu_accounting_locked(accounting);
        task.sched_info().set_cpu_group_locked(group);
        bind_task_group_cpu(task, cpu);
        if fair {
            CompletelyFairScheduler::switched_to_fair(rq, task);
            if queued {
                CompletelyFairScheduler::enqueue(rq, task.clone(), EnqueueFlag::ENQUEUE_WAKEUP);
            }
            if current {
                CompletelyFairScheduler::set_next_task(rq, task.clone());
                rq.resched_current();
            }
        }
        return;
    }
}

use super::Scheduler;
