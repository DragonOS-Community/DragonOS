use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};
use core::{
    fmt::Debug,
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use crate::{
    ipc::sighand::SigHand,
    libs::{
        notifier::{AtomicNotifierChain, NotifierBlock, NotifyResult},
        spinlock::SpinLock,
    },
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
};
use system_error::SystemError;

use super::*;

#[derive(Debug)]
struct RcuSelftestDropProbe {
    id: usize,
    drops: Arc<AtomicUsize>,
}

#[derive(Clone, Copy, Debug)]
enum RcuSelftestNotifyEvent {
    Ping,
}

type RcuSelftestAtomicNotifierChain = AtomicNotifierChain<RcuSelftestNotifyEvent, usize>;
type RcuSelftestNotifierBlock = dyn NotifierBlock<RcuSelftestNotifyEvent, usize>;

#[derive(Debug)]
struct RcuSelftestNotifier {
    id: usize,
    priority: i32,
    ret: i32,
    order: Arc<SpinLock<Vec<usize>>>,
}

impl RcuSelftestNotifier {
    fn new(id: usize, priority: i32, ret: i32, order: Arc<SpinLock<Vec<usize>>>) -> Self {
        Self {
            id,
            priority,
            ret,
            order,
        }
    }
}

impl NotifierBlock<RcuSelftestNotifyEvent, usize> for RcuSelftestNotifier {
    fn notifier_call(&self, _action: RcuSelftestNotifyEvent, data: Option<&usize>) -> i32 {
        if data != Some(&42) {
            return NotifyResult::STOP.bits();
        }

        self.order.lock_irqsave().push(self.id);
        self.ret
    }

    fn priority(&self) -> i32 {
        self.priority
    }
}

struct RcuSelftestReentrantUnregisterNotifier {
    priority: i32,
    chain: Arc<RcuSelftestAtomicNotifierChain>,
    target: SpinLock<Option<Arc<RcuSelftestNotifierBlock>>>,
    result: Arc<AtomicUsize>,
}

impl Debug for RcuSelftestReentrantUnregisterNotifier {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RcuSelftestReentrantUnregisterNotifier")
            .field("priority", &self.priority)
            .finish_non_exhaustive()
    }
}

impl NotifierBlock<RcuSelftestNotifyEvent, usize> for RcuSelftestReentrantUnregisterNotifier {
    fn notifier_call(&self, _action: RcuSelftestNotifyEvent, _data: Option<&usize>) -> i32 {
        let target = self.target.lock_irqsave().clone();
        let Some(target) = target else {
            self.result.store(3, Ordering::SeqCst);
            return NotifyResult::DONE.bits();
        };

        match self.chain.unregister(target) {
            Err(SystemError::EDEADLK_OR_EDEADLOCK) => self.result.store(1, Ordering::SeqCst),
            _ => self.result.store(3, Ordering::SeqCst),
        }

        NotifyResult::DONE.bits()
    }

    fn priority(&self) -> i32 {
        self.priority
    }
}

impl Drop for RcuSelftestDropProbe {
    fn drop(&mut self) {
        let _ = self.id;
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[repr(C)]
struct RcuSelftestCallbackProbe {
    head: RcuHead,
    hits: Arc<AtomicUsize>,
}

unsafe fn rcu_selftest_callback(head: NonNull<RcuHead>) {
    // SAFETY: `head` points to the first field of `RcuSelftestCallbackProbe`,
    // which is allocated by `Box::into_raw()` in the selftest.
    let probe = unsafe { Box::from_raw(head.as_ptr() as *mut RcuSelftestCallbackProbe) };
    probe.hits.fetch_add(1, Ordering::SeqCst);
}

fn force_current_cpu_active_for_selftest(cpu: ProcessorId) {
    let wake_worker = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        let cpu_idx = cpu.data() as usize;
        inner.cpu_states[cpu_idx].in_idle_eqs = false;
        inner.cpu_states[cpu_idx].irq_nesting = 0;
        inner.cpu_states[cpu_idx].irq_from_idle_eqs = false;
        if inner.gp_active && inner.waiting_cpus.get(cpu).unwrap_or(false) {
            inner.waiting_cpus.set(cpu, false);
        }
        RcuState::pump_grace_periods(&mut inner) || inner.has_ready_work()
    };

    RCU_STATE.wake_state_waiters();
    if wake_worker {
        RCU_STATE.wake_worker();
        RCU_STATE.maybe_process_ready_callbacks_inline();
    }
}

fn run_idle_irq_wakeup_selftest() -> Result<(), &'static str> {
    let cpu = smp_get_processor_id();
    let cpu_idx = cpu.data() as usize;

    rcu_barrier();
    let (_, _, _, pending_before, ready_before) = debug_snapshot();
    if pending_before != 0 || ready_before != 0 {
        return Err("rcu callback queues were not empty before idle IRQ selftest");
    }

    let wake_worker = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        let wake_worker = enter_cpu_idle_eqs(&mut inner, cpu);
        if !cpu_in_idle_eqs(&inner.cpu_states[cpu_idx]) {
            return Err("internal idle EQS helper failed to mark the CPU idle");
        }

        let cpu_state = &mut inner.cpu_states[cpu_idx];
        cpu_state.irq_from_idle_eqs = cpu_in_idle_eqs(cpu_state);
        cpu_state.irq_nesting += 1;
        if cpu_in_idle_eqs(cpu_state) {
            return Err("internal idle IRQ helper failed to exit the idle EQS state");
        }

        wake_worker
    };

    RCU_STATE.wake_state_waiters();
    if wake_worker {
        RCU_STATE.wake_worker();
        RCU_STATE.maybe_process_ready_callbacks_inline();
    }

    let idle_hits = Arc::new(AtomicUsize::new(0));
    rcu_defer({
        let idle_hits = idle_hits.clone();
        move || {
            idle_hits.fetch_add(1, Ordering::SeqCst);
        }
    });

    let pre_exit_error = {
        let inner = RCU_STATE.inner.lock_irqsave();
        if idle_hits.load(Ordering::SeqCst) != 0 {
            Some("idle IRQ callback ran before the interrupted CPU returned to idle")
        } else if !inner.gp_active || !inner.waiting_cpus.get(cpu).unwrap_or(false) {
            Some("idle IRQ selftest did not put the current CPU in waiting_cpus")
        } else if inner.pending_callbacks.len() != 1 || !inner.ready_callbacks.is_empty() {
            Some("idle IRQ selftest corrupted callback queue state before irq_exit")
        } else {
            None
        }
    };
    if let Some(error) = pre_exit_error {
        force_current_cpu_active_for_selftest(cpu);
        return Err(error);
    }

    let idle_irq_exit_result = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        let cpu_state = &mut inner.cpu_states[cpu_idx];
        if cpu_state.irq_nesting != 1 || !cpu_state.irq_from_idle_eqs {
            Err("idle IRQ selftest lost the interrupted-idle state")
        } else {
            cpu_state.irq_nesting -= 1;
            cpu_state.irq_from_idle_eqs = false;
            Ok(enter_cpu_idle_eqs(&mut inner, cpu))
        }
    };
    let wake_worker = match idle_irq_exit_result {
        Ok(wake_worker) => wake_worker,
        Err(error) => {
            force_current_cpu_active_for_selftest(cpu);
            return Err(error);
        }
    };

    RCU_STATE.wake_state_waiters();
    if wake_worker {
        RCU_STATE.wake_worker();
        RCU_STATE.maybe_process_ready_callbacks_inline();
    }

    let post_exit_waiting = {
        let inner = RCU_STATE.inner.lock_irqsave();
        inner.waiting_cpus.get(cpu).unwrap_or(false)
    };
    if post_exit_waiting {
        force_current_cpu_active_for_selftest(cpu);
        return Err("idle IRQ exit left the current CPU in waiting_cpus");
    }

    rcu_barrier();
    force_current_cpu_active_for_selftest(cpu);

    if idle_hits.load(Ordering::SeqCst) != 1 {
        return Err("callback did not execute after the idle IRQ wakeup selftest finished");
    }

    Ok(())
}

fn run_pr1_selftest() -> Result<(), &'static str> {
    if ProcessManager::current_pcb().rcu_read_depth() != 0 {
        return Err("initial rcu_read_depth was not zero");
    }

    {
        let _outer = rcu_read_lock();
        if ProcessManager::current_pcb().rcu_read_depth() != 1 {
            return Err("outer rcu_read_lock depth mismatch");
        }

        {
            let _inner = rcu_read_lock();
            if ProcessManager::current_pcb().rcu_read_depth() != 2 {
                return Err("nested rcu_read_lock depth mismatch");
            }
        }

        if ProcessManager::current_pcb().rcu_read_depth() != 1 {
            return Err("nested rcu_read_unlock depth mismatch");
        }
    }

    if ProcessManager::current_pcb().rcu_read_depth() != 0 {
        return Err("final rcu_read_depth was not zero");
    }

    rcu_barrier();
    let (_, completed_gp_before, completed_cb_before, pending_before, ready_before) =
        debug_snapshot();
    if pending_before != 0 || ready_before != 0 {
        return Err("rcu callback queues were not empty before blocked-reader selftest");
    }

    let blocked_hits = Arc::new(AtomicUsize::new(0));
    let blocked_result = {
        let _guard = rcu_read_lock();
        rcu_defer({
            let blocked_hits = blocked_hits.clone();
            move || {
                blocked_hits.fetch_add(1, Ordering::SeqCst);
            }
        });

        if blocked_hits.load(Ordering::SeqCst) != 0 {
            Err("rcu_defer callback ran before leaving the read-side critical section")
        } else {
            note_context_switch();
            let (_, completed_gp_mid, completed_cb_mid, pending_mid, ready_mid) = debug_snapshot();

            if blocked_hits.load(Ordering::SeqCst) != 0 {
                Err("context switch inside rcu_read_lock executed callback early")
            } else if completed_gp_mid != completed_gp_before {
                Err("context switch inside rcu_read_lock incorrectly completed a grace period")
            } else if completed_cb_mid != completed_cb_before {
                Err("context switch inside rcu_read_lock incorrectly completed a callback")
            } else if pending_mid != 1 || ready_mid != 0 {
                Err("context switch inside rcu_read_lock corrupted callback queue state")
            } else {
                Ok(())
            }
        }
    };

    note_context_switch();
    rcu_barrier();
    blocked_result?;

    if blocked_hits.load(Ordering::SeqCst) != 1 {
        return Err("callback did not execute after the blocked reader left its critical section");
    }

    rcu_barrier();
    run_idle_irq_wakeup_selftest()?;

    let callback_hits = Arc::new(AtomicUsize::new(0));
    let callback_probe = Box::new(RcuSelftestCallbackProbe {
        head: RcuHead::new(),
        hits: callback_hits.clone(),
    });
    let callback_probe = Box::into_raw(callback_probe);

    // SAFETY: `callback_probe` stays alive until `rcu_selftest_callback()`
    // reconstructs and consumes the allocation.
    unsafe {
        call_rcu_raw(
            NonNull::new_unchecked(ptr::addr_of_mut!((*callback_probe).head)),
            rcu_selftest_callback,
        );
    }

    rcu_barrier();

    if callback_hits.load(Ordering::SeqCst) != 1 {
        return Err("call_rcu callback was not executed exactly once");
    }

    let deferred_drops = Arc::new(AtomicUsize::new(0));
    rcu_defer_drop(RcuSelftestDropProbe {
        id: 1,
        drops: deferred_drops.clone(),
    });
    rcu_barrier();

    if deferred_drops.load(Ordering::SeqCst) != 1 {
        return Err("rcu_defer_drop did not run after rcu_barrier");
    }

    let deferred_hits = Arc::new(AtomicUsize::new(0));
    rcu_defer({
        let deferred_hits = deferred_hits.clone();
        move || {
            deferred_hits.fetch_add(1, Ordering::SeqCst);
        }
    });
    rcu_barrier();

    if deferred_hits.load(Ordering::SeqCst) != 1 {
        return Err("rcu_defer closure did not run after rcu_barrier");
    }

    Ok(())
}

fn run_pr2_selftest() -> Result<(), &'static str> {
    let old_drops = Arc::new(AtomicUsize::new(0));
    let new_drops = Arc::new(AtomicUsize::new(0));

    let slot = RcuArcSlot::new(Arc::new(RcuSelftestDropProbe {
        id: 1,
        drops: old_drops.clone(),
    }));
    let pinned_old = slot.load();
    if pinned_old.id != 1 {
        return Err("RcuArcSlot::load did not return the published object");
    }

    slot.store_deferred(Arc::new(RcuSelftestDropProbe {
        id: 2,
        drops: new_drops.clone(),
    }));
    rcu_barrier();

    if old_drops.load(Ordering::SeqCst) != 0 {
        return Err("old slot object dropped while a pinned Arc was still alive");
    }

    if slot.load().id != 2 {
        return Err("RcuArcSlot did not publish the replacement object");
    }

    drop(pinned_old);
    if old_drops.load(Ordering::SeqCst) != 1 {
        return Err("old slot object was not dropped after the final pin was released");
    }

    drop(slot);
    rcu_barrier();
    if new_drops.load(Ordering::SeqCst) != 1 {
        return Err("current slot object was not dropped after slot destruction grace period");
    }

    let with_read_old_drops = Arc::new(AtomicUsize::new(0));
    let with_read_new_drops = Arc::new(AtomicUsize::new(0));
    let with_read_slot = RcuArcSlot::new(Arc::new(RcuSelftestDropProbe {
        id: 9,
        drops: with_read_old_drops.clone(),
    }));

    let observed_id = with_read_slot.with_read(|old| {
        with_read_slot.store_deferred(Arc::new(RcuSelftestDropProbe {
            id: 10,
            drops: with_read_new_drops.clone(),
        }));

        if with_read_old_drops.load(Ordering::SeqCst) != 0 {
            return 0;
        }

        old.id
    });
    if observed_id != 9 {
        return Err("RcuArcSlot::with_read did not pin the old snapshot during replacement");
    }

    rcu_barrier();
    if with_read_old_drops.load(Ordering::SeqCst) != 1 {
        return Err("RcuArcSlot::with_read old snapshot was not dropped after the read section");
    }

    drop(with_read_slot);
    rcu_barrier();
    if with_read_new_drops.load(Ordering::SeqCst) != 1 {
        return Err("RcuArcSlot::with_read replacement snapshot was not dropped after slot drop");
    }

    let sighand = SigHand::new();
    if sighand.is_shared() {
        return Err("fresh sighand unexpectedly reported shared");
    }

    sighand.attach_task_ref();
    if sighand.load_count() != 1 || sighand.is_shared() {
        return Err("single task sighand reference tracking is broken");
    }

    let transient_pin = sighand.clone();
    drop(transient_pin);
    if sighand.load_count() != 1 {
        return Err("temporary Arc pin changed sighand task reference count");
    }

    sighand.attach_task_ref();
    if !sighand.is_shared() {
        return Err("double-attached sighand did not report shared");
    }

    sighand.detach_task_ref();
    if sighand.is_shared() || sighand.load_count() != 1 {
        return Err("sighand detach did not restore single-task state");
    }

    sighand.detach_task_ref();
    if sighand.load_count() != 0 {
        return Err("sighand task reference count did not return to zero");
    }

    Ok(())
}

fn run_pr3_selftest() -> Result<(), &'static str> {
    let option_drops = Arc::new(AtomicUsize::new(0));
    let option_replacement_drops = Arc::new(AtomicUsize::new(0));
    let option_clear_drops = Arc::new(AtomicUsize::new(0));
    let option_race_old_drops = Arc::new(AtomicUsize::new(0));
    let option_race_new_drops = Arc::new(AtomicUsize::new(0));
    let option_drop_drops = Arc::new(AtomicUsize::new(0));

    let option_slot = RcuOptionArcSlot::new_none();
    if option_slot.load().is_some() {
        return Err("RcuOptionArcSlot::new_none did not start empty");
    }

    option_slot.store_deferred(Some(Arc::new(RcuSelftestDropProbe {
        id: 3,
        drops: option_drops.clone(),
    })));
    let pinned_option = option_slot
        .load()
        .ok_or("RcuOptionArcSlot did not publish the first object")?;
    if pinned_option.id != 3 {
        return Err("RcuOptionArcSlot loaded the wrong first object");
    }

    option_slot.store_deferred(Some(Arc::new(RcuSelftestDropProbe {
        id: 4,
        drops: option_replacement_drops.clone(),
    })));
    rcu_barrier();
    if option_drops.load(Ordering::SeqCst) != 0 {
        return Err("RcuOptionArcSlot dropped a pinned old object");
    }
    if option_slot.load().map(|value| value.id) != Some(4) {
        return Err("RcuOptionArcSlot did not publish the replacement object");
    }

    drop(pinned_option);
    if option_drops.load(Ordering::SeqCst) != 1 {
        return Err("RcuOptionArcSlot old object was not dropped after final pin");
    }

    option_slot.store_deferred(None);
    rcu_barrier();
    if option_slot.load().is_some() {
        return Err("RcuOptionArcSlot did not clear to None");
    }
    if option_replacement_drops.load(Ordering::SeqCst) != 1 {
        return Err("RcuOptionArcSlot replacement object was not dropped after clear");
    }

    option_slot.store_deferred(Some(Arc::new(RcuSelftestDropProbe {
        id: 5,
        drops: option_clear_drops.clone(),
    })));
    if !option_slot.clear_if_deferred(|value| value.id == 5) {
        return Err("RcuOptionArcSlot clear_if_deferred did not clear a matching object");
    }
    rcu_barrier();
    if option_slot.load().is_some() {
        return Err("RcuOptionArcSlot clear_if_deferred left a matching object published");
    }
    if option_clear_drops.load(Ordering::SeqCst) != 1 {
        return Err("RcuOptionArcSlot clear_if_deferred did not drop the cleared object");
    }

    option_slot.store_deferred(Some(Arc::new(RcuSelftestDropProbe {
        id: 6,
        drops: option_race_old_drops.clone(),
    })));
    let replaced_once = AtomicBool::new(false);
    let cleared = option_slot.clear_if_deferred(|value| {
        if value.id == 6 && !replaced_once.swap(true, Ordering::SeqCst) {
            option_slot.store_deferred(Some(Arc::new(RcuSelftestDropProbe {
                id: 7,
                drops: option_race_new_drops.clone(),
            })));
            return true;
        }

        value.id == 6
    });
    if cleared {
        return Err("RcuOptionArcSlot clear_if_deferred cleared after a racing replacement");
    }
    if option_slot.load().map(|value| value.id) != Some(7) {
        return Err("RcuOptionArcSlot clear_if_deferred lost the racing replacement");
    }
    rcu_barrier();
    if option_race_old_drops.load(Ordering::SeqCst) != 1 {
        return Err("RcuOptionArcSlot racing old object was not dropped");
    }
    if option_race_new_drops.load(Ordering::SeqCst) != 0 {
        return Err("RcuOptionArcSlot racing replacement was dropped unexpectedly");
    }

    option_slot.store_deferred(None);
    rcu_barrier();
    if option_race_new_drops.load(Ordering::SeqCst) != 1 {
        return Err("RcuOptionArcSlot racing replacement was not dropped after final clear");
    }

    let drop_slot = RcuOptionArcSlot::new_some(Arc::new(RcuSelftestDropProbe {
        id: 8,
        drops: option_drop_drops.clone(),
    }));
    let pinned_drop_slot = drop_slot
        .load()
        .ok_or("RcuOptionArcSlot drop test did not publish an object")?;
    drop(drop_slot);
    rcu_barrier();
    if option_drop_drops.load(Ordering::SeqCst) != 0 {
        return Err("RcuOptionArcSlot drop released an object with a live reader pin");
    }

    drop(pinned_drop_slot);
    if option_drop_drops.load(Ordering::SeqCst) != 1 {
        return Err("RcuOptionArcSlot drop object was not released after final reader pin");
    }

    Ok(())
}

fn check_notifier_order(
    order: &Arc<SpinLock<Vec<usize>>>,
    expected: &[usize],
    reason: &'static str,
) -> Result<(), &'static str> {
    let observed = order.lock_irqsave().clone();
    if observed.as_slice() != expected {
        return Err(reason);
    }

    Ok(())
}

fn clear_notifier_order(order: &Arc<SpinLock<Vec<usize>>>) {
    order.lock_irqsave().clear();
}

fn run_pr5_selftest() -> Result<(), &'static str> {
    let chain = Arc::new(RcuSelftestAtomicNotifierChain::new());
    let order = Arc::new(SpinLock::new(Vec::new()));
    let data = 42;

    let high: Arc<RcuSelftestNotifierBlock> = Arc::new(RcuSelftestNotifier::new(
        1,
        20,
        NotifyResult::OK.bits(),
        order.clone(),
    ));
    let low: Arc<RcuSelftestNotifierBlock> = Arc::new(RcuSelftestNotifier::new(
        2,
        10,
        NotifyResult::DONE.bits(),
        order.clone(),
    ));
    let same_prio: Arc<RcuSelftestNotifierBlock> = Arc::new(RcuSelftestNotifier::new(
        3,
        20,
        NotifyResult::DONE.bits(),
        order.clone(),
    ));
    let stop: Arc<RcuSelftestNotifierBlock> = Arc::new(RcuSelftestNotifier::new(
        4,
        15,
        NotifyResult::STOP.bits(),
        order.clone(),
    ));

    chain
        .register(low.clone())
        .map_err(|_| "atomic notifier failed to register the low-priority block")?;
    chain
        .register(high.clone())
        .map_err(|_| "atomic notifier failed to register the high-priority block")?;

    match chain.register(high.clone()) {
        Err(SystemError::EEXIST) => {}
        _ => return Err("atomic notifier accepted a duplicated block registration"),
    }

    match chain.register_unique_prio(same_prio.clone()) {
        Err(SystemError::EBUSY) => {}
        _ => return Err("atomic notifier accepted a duplicate unique priority"),
    }

    let (ret, nr_calls) = chain.call_chain(RcuSelftestNotifyEvent::Ping, Some(&data), None);
    if ret != NotifyResult::DONE.bits() || nr_calls != 2 {
        return Err("atomic notifier full call_chain returned the wrong result");
    }
    check_notifier_order(
        &order,
        &[1, 2],
        "atomic notifier did not dispatch in priority order",
    )?;

    clear_notifier_order(&order);
    let (ret, nr_calls) = chain.call_chain(RcuSelftestNotifyEvent::Ping, Some(&data), Some(1));
    if ret != NotifyResult::OK.bits() || nr_calls != 1 {
        return Err("atomic notifier nr_to_call did not stop after one callback");
    }
    check_notifier_order(
        &order,
        &[1],
        "atomic notifier nr_to_call dispatched the wrong callbacks",
    )?;

    chain
        .register(stop.clone())
        .map_err(|_| "atomic notifier failed to register the stop block")?;

    clear_notifier_order(&order);
    let (ret, nr_calls) = chain.call_chain(RcuSelftestNotifyEvent::Ping, Some(&data), None);
    if !NotifyResult::from_bits_truncate(ret).contains(NotifyResult::STOP_MASK)
        || ret != NotifyResult::STOP.bits()
        || nr_calls != 2
    {
        return Err("atomic notifier did not honor NOTIFY_STOP_MASK");
    }
    check_notifier_order(
        &order,
        &[1, 4],
        "atomic notifier continued after a NOTIFY_STOP result",
    )?;

    chain
        .unregister(stop.clone())
        .map_err(|_| "atomic notifier failed to unregister the stop block")?;

    clear_notifier_order(&order);
    let (ret, nr_calls) = chain.call_chain(RcuSelftestNotifyEvent::Ping, Some(&data), None);
    if ret != NotifyResult::DONE.bits() || nr_calls != 2 {
        return Err("atomic notifier unregister did not publish the replacement snapshot");
    }
    check_notifier_order(
        &order,
        &[1, 2],
        "atomic notifier still dispatched an unregistered block",
    )?;

    let reentrant_result = Arc::new(AtomicUsize::new(0));
    let reentrant = Arc::new(RcuSelftestReentrantUnregisterNotifier {
        priority: 30,
        chain: chain.clone(),
        target: SpinLock::new(None),
        result: reentrant_result.clone(),
    });
    let reentrant_block: Arc<RcuSelftestNotifierBlock> = reentrant.clone();
    *reentrant.target.lock_irqsave() = Some(reentrant_block.clone());

    chain
        .register(reentrant_block.clone())
        .map_err(|_| "atomic notifier failed to register the reentrant block")?;

    let _ = chain.call_chain(RcuSelftestNotifyEvent::Ping, Some(&data), Some(1));
    if reentrant_result.load(Ordering::SeqCst) != 1 {
        return Err("atomic notifier unregister from call_chain did not return EDEADLK");
    }

    chain
        .unregister(reentrant_block)
        .map_err(|_| "atomic notifier failed to unregister the reentrant block afterward")?;
    chain
        .unregister(high)
        .map_err(|_| "atomic notifier failed to unregister the high-priority block")?;
    chain
        .unregister(low)
        .map_err(|_| "atomic notifier failed to unregister the low-priority block")?;

    Ok(())
}

pub fn run_debug_selftests() -> String {
    let pr1 = run_pr1_selftest();
    let pr2 = run_pr2_selftest();
    let pr3 = run_pr3_selftest();
    let pr5 = run_pr5_selftest();
    let overall_ok = pr1.is_ok() && pr2.is_ok() && pr3.is_ok() && pr5.is_ok();

    let mut report = String::new();
    report.push_str(if overall_ok {
        "status=ok\n"
    } else {
        "status=fail\n"
    });

    match pr1 {
        Ok(()) => report.push_str("pr1=ok\n"),
        Err(reason) => report.push_str(&format!("pr1=fail:{reason}\n")),
    }

    match pr2 {
        Ok(()) => report.push_str("pr2=ok\n"),
        Err(reason) => report.push_str(&format!("pr2=fail:{reason}\n")),
    }

    match pr3 {
        Ok(()) => report.push_str("pr3=ok\n"),
        Err(reason) => report.push_str(&format!("pr3=fail:{reason}\n")),
    }

    match pr5 {
        Ok(()) => report.push_str("pr5=ok\n"),
        Err(reason) => report.push_str(&format!("pr5=fail:{reason}\n")),
    }

    report
}
