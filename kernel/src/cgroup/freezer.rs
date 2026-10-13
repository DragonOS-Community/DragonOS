//! cgroup v2 freezer: requests, safe task acknowledgements and membership.
//!
//! UPDATE/accounting -> STATE -> task freezer; parking/stop: pi -> STATE.
//! Safe points never take UPDATE/accounting or read task_cgroup. Wakes and
//! scheduler placement happen after STATE is released. No signal is synthesized.
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use system_error::SystemError;

use crate::{
    arch::{ipc::signal::Signal, CurrentIrqArch},
    exception::InterruptArch,
    libs::spinlock::SpinLock,
    process::{ProcessControlBlock, ProcessFlags, ProcessManager, ProcessState},
    sched::{schedule, SchedMode},
};

use super::CgroupNode;

static STATE: SpinLock<()> = SpinLock::new(());

/// Acknowledge exactly the record read, never a later producer's notification.
pub(crate) fn snapshot_events(node: &CgroupNode) -> (bool, bool, u64) {
    let _state = STATE.lock_irqsave();
    (
        node.is_populated(),
        node.frozen(),
        node.core_events_sequence(),
    )
}

/// Atomic reader snapshots; all writers hold STATE. Population's denominator
/// is the existing inclusive live-task count, not a second membership registry.
#[derive(Debug, Default)]
pub(crate) struct NodeState {
    requested: AtomicBool,
    effective: AtomicUsize,
    acknowledged: AtomicUsize,
    frozen: AtomicBool,
}

impl NodeState {
    pub(crate) fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    pub(crate) fn frozen(&self) -> bool {
        self.frozen.load(Ordering::Acquire)
    }

    fn effective(&self) -> bool {
        self.effective.load(Ordering::Acquire) != 0
    }
}

/// The stable node is only installed for published live membership. This
/// narrow cache lets switch-tail acknowledgement avoid sleeping configuration
/// locks. Stopped/vfork acknowledgements exist independently of a request.
#[derive(Debug, Default)]
struct Membership {
    node: Option<Arc<CgroupNode>>,
    acknowledged: bool,
    safe_wait: bool,
}

#[derive(Debug)]
pub(crate) struct TaskState {
    membership: SpinLock<Membership>,
    /// Scheduler sleep cancellation reads this without taking STATE or pi.
    pub(crate) parked: AtomicBool,
}

impl Default for TaskState {
    fn default() -> Self {
        Self {
            membership: SpinLock::new(Membership::default()),
            parked: AtomicBool::new(false),
        }
    }
}

fn revisit(node: &CgroupNode) {
    let frozen = node.freezer.effective()
        && node.freezer.acknowledged.load(Ordering::Relaxed) == node.subtree_task_count();
    if node.freezer.frozen.swap(frozen, Ordering::AcqRel) != frozen {
        node.notify_core_events();
    }
}

fn revisit_tree(node: &Arc<CgroupNode>) {
    let mut cursor = Some(node.clone());
    while let Some(node) = cursor {
        revisit(&node);
        cursor = node.parent();
    }
}

fn acknowledge(member: &mut Membership, value: bool) {
    if member.acknowledged == value {
        return;
    }
    let Some(node) = member.node.as_ref() else {
        return;
    };
    let mut cursor = Some(node.clone());
    while let Some(current) = cursor {
        if value {
            current.freezer.acknowledged.fetch_add(1, Ordering::Relaxed);
        } else {
            let old = current.freezer.acknowledged.fetch_sub(1, Ordering::Relaxed);
            debug_assert_ne!(old, 0, "freezer acknowledgement underflow");
        }
        cursor = current.parent();
    }
    member.acknowledged = value;
}

fn publish_work(task: &ProcessControlBlock, member: &Membership) {
    if !task.is_kthread()
        && (member.acknowledged || member.node.as_ref().is_some_and(|n| n.freezer.effective()))
    {
        task.flags().insert(ProcessFlags::NEED_CGROUP_FREEZE);
    } else {
        task.flags().remove(ProcessFlags::NEED_CGROUP_FREEZE);
    }
}

/// UPDATE excludes topology/request changes during a child's preparation.
pub(crate) fn initialize(node: &CgroupNode, parent: &CgroupNode) {
    let _state = STATE.lock_irqsave();
    node.freezer.effective.store(
        parent.freezer.effective.load(Ordering::Relaxed),
        Ordering::Release,
    );
    revisit(node);
}

/// Caller holds the existing accounting/membership transaction.
pub(crate) fn publish(task: &ProcessControlBlock, node: Arc<CgroupNode>) {
    let _state = STATE.lock_irqsave();
    let mut member = task.freezer.membership.lock_irqsave();
    debug_assert!(member.node.is_none());
    node.add_task(task.raw_pid());
    member.node = Some(node.clone());
    publish_work(task, &member);
    revisit_tree(&node);
}

/// Transfer a live task and its safe acknowledgement in one transaction.
/// Caller publishes task_cgroup afterwards, still under accounting.
pub(crate) fn migrate(task: &ProcessControlBlock, old: &Arc<CgroupNode>, new: &Arc<CgroupNode>) {
    let _state = STATE.lock_irqsave();
    let mut member = task.freezer.membership.lock_irqsave();
    debug_assert!(member.node.as_ref().is_some_and(|n| Arc::ptr_eq(n, old)));
    let acknowledged = member.acknowledged;
    acknowledge(&mut member, false);
    new.add_task(task.raw_pid());
    old.remove_task(task.raw_pid());
    member.node = Some(new.clone());
    acknowledge(&mut member, acknowledged);
    publish_work(task, &member);
    revisit_tree(old);
    revisit_tree(new);
}

pub(crate) fn exit(task: &ProcessControlBlock) {
    let retired = {
        let _state = STATE.lock_irqsave();
        let mut member = task.freezer.membership.lock_irqsave();
        acknowledge(&mut member, false);
        let node = member
            .node
            .take()
            .expect("exiting task has live cgroup membership");
        node.remove_task(task.raw_pid());
        task.freezer.parked.store(false, Ordering::Release);
        task.flags().remove(ProcessFlags::NEED_CGROUP_FREEZE);
        revisit_tree(&node);
        node
    };
    drop(retired);
}

/// No STATE/accounting/pi/rq lock is held here. Do not disturb job-control stop
/// or uninterruptible I/O. A request is persistent across wake-before-sleep.
pub(crate) fn wake_for_request(task: &Arc<ProcessControlBlock>) {
    if task.is_kthread()
        || (!task.flags().contains(ProcessFlags::NEED_CGROUP_FREEZE)
            && !task.freezer.parked.load(Ordering::Acquire))
    {
        return;
    }
    if task.sched_info().state().is_blocked_interruptable() {
        let _ = ProcessManager::wakeup(task);
    } else if task.sched_info().state().is_runnable() {
        ProcessManager::kick(task);
    }
}

/// Caller holds UPDATE; prepare every allocation before changing any request.
pub(crate) fn set_requested(node: &Arc<CgroupNode>, freeze: bool) -> Result<(), SystemError> {
    if node.freeze_requested() == freeze {
        return Ok(());
    }
    let mut nodes = Vec::new();
    nodes.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
    nodes.push(node.clone());
    let mut index = 0;
    let mut tasks = Vec::new();
    while index < nodes.len() {
        let current = nodes[index].clone();
        let children = current.children_snapshot()?;
        nodes
            .try_reserve(children.len())
            .map_err(|_| SystemError::ENOMEM)?;
        nodes.extend(children);
        let pids = current.tasks_snapshot()?;
        tasks
            .try_reserve(pids.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for pid in pids {
            if let Some(task) = ProcessManager::find(pid) {
                tasks.push(task);
            }
        }
        index += 1;
    }
    {
        let _accounting = super::cgroup_accounting_lock().lock();
        let _state = STATE.lock_irqsave();
        node.freezer.requested.store(freeze, Ordering::Release);
        for current in &nodes {
            if freeze {
                current.freezer.effective.fetch_add(1, Ordering::AcqRel);
            } else {
                let old = current.freezer.effective.fetch_sub(1, Ordering::AcqRel);
                debug_assert_ne!(old, 0);
            }
            revisit(current);
        }
        for task in &tasks {
            let member = task.freezer.membership.lock_irqsave();
            publish_work(task, &member);
        }
        revisit_tree(node);
        // Linux also notifies when an ancestor enforces the same effective
        // state, so the writer need not wait forever for a nonexistent edge.
        node.notify_core_events();
    }
    for task in tasks {
        if freeze || task.freezer.parked.load(Ordering::Acquire) {
            wake_for_request(&task);
        }
    }
    Ok(())
}

/// Called after the switch to a different stack, before publishing that the
/// previous context is reusable, without holding rq/pi/architecture locks.
pub(crate) fn switched_out(task: &ProcessControlBlock) {
    if task.is_kthread() || !task.sched_info().state().is_stopped() {
        return;
    }
    let _state = STATE.lock_irqsave();
    if !task.sched_info().state().is_stopped() {
        return;
    }
    let mut member = task.freezer.membership.lock_irqsave();
    acknowledge(&mut member, true);
    publish_work(task, &member);
    if let Some(node) = &member.node {
        revisit_tree(node);
    }
}

/// stop_task holds pi but has released rq. Placement is not execution
/// ownership: a persistent on_cpu() does not mean this context is running.
pub(crate) fn stopped_off_cpu_locked(task: &ProcessControlBlock) {
    if !task.sched_info().is_running() {
        switched_out(task);
    }
}

/// vfork's completion wait is a Linux-defined safe wait, not ordinary D-state.
pub(crate) fn begin_safe_wait(task: &ProcessControlBlock) {
    let _state = STATE.lock_irqsave();
    let mut member = task.freezer.membership.lock_irqsave();
    member.safe_wait = true;
    acknowledge(&mut member, true);
    publish_work(task, &member);
    if let Some(node) = &member.node {
        revisit_tree(node);
    }
}

pub(crate) fn end_safe_wait(task: &ProcessControlBlock) {
    let _state = STATE.lock_irqsave();
    let mut member = task.freezer.membership.lock_irqsave();
    member.safe_wait = false;
    if !member.node.as_ref().is_some_and(|n| n.freezer.effective()) {
        acknowledge(&mut member, false);
    }
    publish_work(task, &member);
    if let Some(node) = &member.node {
        revisit_tree(node);
    }
}

/// Lock-free user boundary, never called while holding a subsystem guard.
/// It may return for a fatal signal or ptrace trap, which the caller services
/// before revisiting this boundary. Normal signals cannot escape the freezer.
pub(crate) fn handle_user_return() {
    let task = ProcessManager::current_pcb();
    if !task.flags().contains(ProcessFlags::NEED_CGROUP_FREEZE) {
        return;
    }
    loop {
        let fatal = Signal::oom_fatal_signal_pending(&task);
        let stop_pending = task
            .flags()
            .intersects(ProcessFlags::PENDING_PTRACE_STOP | ProcessFlags::PENDING_GROUP_STOP);
        let sleep = {
            let _pi = task.sched_info().pi_lock_irqsave();
            let _state = STATE.lock_irqsave();
            let mut member = task.freezer.membership.lock_irqsave();
            let freezing = member.node.as_ref().is_some_and(|n| n.freezer.effective());
            let sleep = if fatal
                || stop_pending
                || !freezing
                || task.is_kthread()
                || task.sched_info().state().is_exited()
            {
                task.freezer.parked.store(false, Ordering::Release);
                if !member.safe_wait {
                    acknowledge(&mut member, false);
                }
                if fatal || !freezing {
                    task.flags().remove(ProcessFlags::NEED_CGROUP_FREEZE);
                }
                false
            } else {
                task.freezer.parked.store(true, Ordering::Release);
                if !task.sched_info().state().is_stopped() {
                    task.sched_info().set_state(ProcessState::Blocked(true));
                    task.flags().insert(ProcessFlags::NEED_SCHEDULE);
                }
                acknowledge(&mut member, true);
                true
            };
            if let Some(node) = &member.node {
                revisit_tree(node);
            }
            sleep
        };
        if !sleep {
            return;
        }
        unsafe {
            CurrentIrqArch::interrupt_enable();
        }
        schedule(SchedMode::SM_NONE);
    }
}
