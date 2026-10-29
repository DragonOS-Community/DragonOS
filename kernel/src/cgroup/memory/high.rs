//! Task-context memory.high backpressure with prepared deadline storage.
//!
//! Charge paths only add debt and return-user work. Deadline IRQ callbacks
//! never upgrade a user PCB; the permanently pinned memory notifier performs
//! the actual wake in task context. Preparation happens at user entry before
//! resource locks, including for root tasks which can migrate while blocked.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use system_error::SystemError;

use crate::{
    arch::ipc::signal::Signal,
    exception::in_interrupt,
    libs::wait_queue::Waker,
    mm::{
        allocator::page_frame::PageFrameCount, memory_notify::weak_registry::WeakRegistry,
        page::PageReclaimer,
    },
    process::{ProcessFlags, ProcessManager},
    smp::{
        core::smp_get_processor_id,
        cpu::{smp_cpu_manager, ProcessorId},
    },
    time::{
        clocksource::HZ,
        deadline::{DeadlineCallback, DeadlineEvent},
        timekeeping::monotonic_now,
    },
};

use super::{MemoryAccount, MemoryEvent};

static WAITERS: WeakRegistry<HighWait> = WeakRegistry::new();

#[derive(Debug)]
struct HighDeadline;
impl DeadlineCallback for HighDeadline {
    fn expire(&self, _now_ns: u64) -> Option<u64> {
        crate::mm::memory_notify::request_high_deadline();
        None
    }
}

#[derive(Debug)]
pub(crate) struct HighWait {
    waker: Arc<Waker>,
    events: Vec<(ProcessorId, Arc<DeadlineEvent>)>,
    deadline: AtomicU64,
    registration: AtomicUsize,
}

impl HighWait {
    fn try_new_for(
        task: &Arc<crate::process::ProcessControlBlock>,
    ) -> Result<Arc<Self>, SystemError> {
        let waker = Waker::try_new_for(task)?;
        let callback = Arc::try_new(HighDeadline).map_err(|_| SystemError::ENOMEM)?;
        let manager = smp_cpu_manager();
        let mut events = Vec::new();
        events
            .try_reserve_exact(manager.possible_cpus_count() as usize)
            .map_err(|_| SystemError::ENOMEM)?;
        // Possible CPUs are fixed at boot on this platform. Prepare an event
        // for each, not only currently online CPUs: initial AP startup must
        // not leave a later migration without a local, reusable event.
        for cpu in manager.possible_cpus().iter_cpu() {
            events.push((
                cpu,
                DeadlineEvent::try_new(cpu, callback.clone()).map_err(|_| SystemError::ENOMEM)?,
            ));
        }
        let state = Arc::try_new(Self {
            waker,
            events,
            deadline: AtomicU64::new(0),
            registration: AtomicUsize::new(0),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        WAITERS.register(&state, |id| state.registration.store(id, Ordering::Release))?;
        Ok(state)
    }

    fn sleep(&self, deadline: u64) -> Result<(), SystemError> {
        // Selection and local arm are one non-sleeping CPU-stable operation.
        // RISC-V does not support remote arm; migration between these two
        // steps must not turn real high pressure into an unsupported no-op.
        let preempt = crate::process::preempt::PreemptGuard::new();
        let cpu = smp_get_processor_id();
        let event = self
            .events
            .iter()
            .find(|(owner, _)| *owner == cpu)
            .map(|(_, event)| event)
            .ok_or(SystemError::EOPNOTSUPP_OR_ENOTSUP)?;
        self.deadline.store(deadline, Ordering::Release);
        if let Err(error) = event.arm(deadline) {
            self.deadline.store(0, Ordering::Release);
            return Err(error);
        }
        drop(preempt);
        let result = (|| {
            while monotonic_now().to_ktime_ns() < deadline {
                self.waker.wait_current_killable().map_err(|error| {
                    if error == SystemError::ERESTARTSYS {
                        SystemError::EINTR
                    } else {
                        error
                    }
                })?;
            }
            Ok(())
        })();
        self.deadline.store(0, Ordering::Release);
        event.cancel();
        result
    }
}

impl Drop for HighWait {
    fn drop(&mut self) {
        self.deadline.store(0, Ordering::Release);
        WAITERS.unregister(self.registration.swap(0, Ordering::AcqRel));
        for (_, event) in &self.events {
            event.cancel();
        }
        self.waker.close();
    }
}

/// Birth before task publication or the first kernel-to-user exec transaction.
/// This deliberately does not exempt KTHREAD: exec callers are preparing a
/// future user task, not charging a kernel worker.
pub(crate) fn prepare_for_task(
    task: &Arc<crate::process::ProcessControlBlock>,
) -> Result<(), SystemError> {
    if task.memory_high_wait.lock_irqsave().is_some() {
        return Ok(());
    }
    let prepared = HighWait::try_new_for(task)?;
    let mut slot = task.memory_high_wait.lock_irqsave();
    if slot.is_none() {
        *slot = Some(prepared.clone());
    }
    drop(slot);
    drop(prepared);
    Ok(())
}

pub(crate) fn current_prepared() -> bool {
    ProcessManager::current_pcb()
        .memory_high_wait
        .lock_irqsave()
        .is_some()
}

/// The caller enables IRQs and holds no resource owners across this function
/// and the subsequent non-unwinding signal boundary. User task birth prepares
/// all fallible storage; only fatal interruption is an ordinary error here.
pub(crate) fn handle_user_return() {
    let needed = ProcessManager::current_pcb()
        .flags()
        .contains(ProcessFlags::NEED_MEMORY_HIGH);
    if !needed {
        return;
    }
    match handle_current() {
        Ok(()) | Err(SystemError::EINTR) => {}
        Err(error) => panic!(
            "prepared memory.high return infrastructure failed: {:?}",
            error
        ),
    }
}

/// Non-sleeping, allocation-free charge-side work. No reclaim or waiter birth.
pub(crate) fn record_charge(account: &MemoryAccount, pages: u64) {
    if pages == 0 || !ProcessManager::initialized() || in_interrupt() {
        return;
    }
    let mut node = account;
    let mut exceeded = false;
    while let Some(parent) = node.parent() {
        if node
            .policy()
            .high
            .is_some_and(|high| node.current_pages() > high)
        {
            exceeded = true;
            break;
        }
        node = parent;
    }
    if !exceeded {
        return;
    }
    record_charge_debt(pages);
}

/// Caller has established an actual newly charged stock increment above high.
/// Ownership transfer must not turn an unchanged common ancestor into debt.
pub(crate) fn record_charge_debt(pages: u64) {
    if pages == 0 || !ProcessManager::initialized() || in_interrupt() {
        return;
    }
    let task = ProcessManager::current_pcb();
    if task.flags().contains(ProcessFlags::KTHREAD) {
        return;
    }
    let _ = task
        .memory_high_debt
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |debt| {
            Some(debt.saturating_add(pages))
        });
    task.flags().insert(ProcessFlags::NEED_MEMORY_HIGH);
}

/// Must finish and drop all ordinary owners before the signal exit boundary.
pub(crate) fn handle_current() -> Result<(), SystemError> {
    let task = ProcessManager::current_pcb();
    task.flags().remove(ProcessFlags::NEED_MEMORY_HIGH);
    let pages = task.memory_high_debt.swap(0, Ordering::AcqRel);
    if pages == 0 || task.flags().contains(ProcessFlags::KTHREAD) {
        return Ok(());
    }
    let result = handle_debt(&task, pages);
    if result.is_err() && result != Err(SystemError::EINTR) {
        // Preparation/arming failure does not pay the allocation's debt.
        // A fatal exit is the only failure which need not retain it.
        let _ = task
            .memory_high_debt
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |debt| {
                Some(debt.saturating_add(pages))
            });
        task.flags().insert(ProcessFlags::NEED_MEMORY_HIGH);
    }
    result
}

fn handle_debt(
    task: &Arc<crate::process::ProcessControlBlock>,
    pages: u64,
) -> Result<(), SystemError> {
    let mm = { task.basic().user_vm() };
    let account = mm
        .as_ref()
        .map_or_else(|| super::account_for_task(task), |mm| mm.memory_account());
    let prepared = task.memory_high_wait.lock_irqsave().clone();
    drop(mm);
    // User-task birth and kernel-to-user exec prepare this before publication
    // or any user allocations. Return-side pressure never births metadata.
    let prepared = prepared.ok_or(SystemError::ENOMEM)?;
    let mut retries = 16u8;
    let mut target_pages = pages;
    loop {
        if Signal::oom_fatal_signal_pending(&ProcessManager::current_pcb()) {
            return Err(SystemError::EINTR);
        }
        let mut node = account.clone();
        let mut reclaimed = 0usize;
        while let Some(parent) = node.parent().cloned() {
            if node.is_online()
                && node
                    .policy()
                    .high
                    .is_some_and(|high| node.current_pages() > high)
            {
                node.record_event(MemoryEvent::High);
                let progress = match PageReclaimer::shrink_account(
                    &node,
                    PageFrameCount::new(target_pages.min(usize::MAX as u64) as usize),
                ) {
                    Ok(progress) => progress.reclaimed,
                    // Protection preparation may fail physically. Preserve
                    // min/low protection by skipping this pass; high is a
                    // soft limit, so bounded penalty (not OOM) still applies.
                    Err(SystemError::ENOMEM) => 0,
                    Err(error) => return Err(error),
                };
                reclaimed = reclaimed.saturating_add(progress);
            }
            node = parent;
        }
        let delay = delay_ns(&account, pages);
        if delay == 0 {
            return Ok(());
        }
        if reclaimed != 0 || retries != 0 {
            if reclaimed == 0 {
                retries -= 1;
            }
            target_pages = 32;
            crate::sched::sched_yield();
            continue;
        }
        return prepared.sleep(monotonic_now().to_ktime_ns().saturating_add(delay));
    }
}

fn delay_ns(account: &MemoryAccount, pages: u64) -> u64 {
    let mut node = account;
    let mut overage = 0u128;
    while let Some(parent) = node.parent() {
        if node.is_online() {
            if let Some(high) = node.policy().high {
                let usage = node.current_pages();
                if usage > high {
                    let threshold = high.max(1);
                    overage = overage
                        .max(((usage.saturating_sub(threshold) as u128) << 20) / threshold as u128);
                }
            }
        }
        node = parent;
    }
    let ticks = ((overage.saturating_mul(overage).saturating_mul(HZ as u128) >> 20) >> 14)
        .saturating_mul(pages as u128)
        / 32;
    let ticks = ticks.min((2 * HZ) as u128) as u64;
    if ticks <= HZ / 100 {
        0
    } else {
        ticks * 1_000_000_000 / HZ
    }
}

/// A deadline IRQ only requests this deferred scan; release notifications do
/// not scan idle high waiters or shorten their Linux-style penalty deadline.
pub(crate) fn notify_deadlines() {
    let now = monotonic_now().to_ktime_ns();
    WAITERS.for_each(
        |state| {
            let deadline = state.deadline.load(Ordering::Acquire);
            if deadline != 0 && now >= deadline {
                state.waker.wake();
            }
        },
        |state| state.registration.load(Ordering::Acquire) != 0,
    );
}
