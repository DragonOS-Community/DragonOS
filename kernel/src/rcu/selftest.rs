use alloc::{boxed::Box, format, string::String, sync::Arc};
use core::{
    ptr::{self, NonNull},
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{
    ipc::sighand::SigHand,
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
};

use super::*;

#[derive(Debug)]
struct RcuSelftestDropProbe {
    id: usize,
    drops: Arc<AtomicUsize>,
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
    if new_drops.load(Ordering::SeqCst) != 1 {
        return Err("current slot object was not dropped when the slot was destroyed");
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

pub fn run_debug_selftests() -> String {
    let pr1 = run_pr1_selftest();
    let pr2 = run_pr2_selftest();
    let overall_ok = pr1.is_ok() && pr2.is_ok();

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

    report
}
