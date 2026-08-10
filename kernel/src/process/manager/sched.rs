use core::sync::atomic::{fence, Ordering};

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::{cpu::current_cpu_id, CurrentIrqArch},
    exception::InterruptArch,
    process::{ProcessControlBlock, ProcessFlags, ProcessManager, ProcessState},
    sched::{
        cpu_rq, enqueue_task_on_cpu, select_task_rq, DequeueFlag, EnqueueFlag, OnRq, SchedPolicy,
        Scheduler, WakeupFlags,
    },
    smp::{core::smp_get_processor_id, kick_cpu},
};

impl ProcessManager {
    /// Wake up a process.
    pub fn wakeup(pcb: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        let _guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let state = pcb.sched_info().state();
        if !state.is_blocked() {
            if state.is_exited() {
                return Err(SystemError::EINVAL);
            }
            return Ok(());
        }

        // Read state under pi_lock protection to determine
        // sched_contributes_to_load.
        let pi_guard = pcb.sched_info().pi_lock_irqsave();
        fence(Ordering::SeqCst); // smp_mb__after_spinlock()
        let state = pcb.sched_info().state();
        if !state.is_blocked() {
            if state.is_exited() {
                return Err(SystemError::EINVAL);
            }
            return Ok(());
        }
        let was_uninterruptible = matches!(state, ProcessState::Blocked(false));

        pcb.sched_info().set_state(ProcessState::Runnable);
        fence(Ordering::SeqCst);

        pcb.debug_assert_fork_cpu_binding();

        if *pcb.sched_info().on_rq.lock_irqsave() == OnRq::Queued {
            if let Some(target_cpu) = pcb.sched_info().on_cpu() {
                let rq = cpu_rq(target_cpu.data() as usize);
                let (rq, _rq_guard) = rq.self_lock();

                // Linux ttwu_runnable(): a blocked-but-still-queued task has
                // not yet been dequeued by schedule(). Recheck on_rq under the
                // target rq lock; if schedule() won the race and dequeued it,
                // fall through to the full enqueue path below.
                if *pcb.sched_info().on_rq.lock_irqsave() == OnRq::Queued {
                    if !Arc::ptr_eq(&rq.current(), pcb) {
                        rq.update_rq_clock();
                        rq.check_preempt_current(pcb, WakeupFlags::WF_TTWU);
                    }
                    return Ok(());
                }
            }
        }

        let prev_cpu = pcb.sched_info().on_cpu().unwrap_or(current_cpu_id());
        // Linux ttwu waits for p->on_cpu after observing an off-rq task. The
        // old CPU may already have dequeued this task but still be executing
        // switch_process() on its kernel stack. Enqueuing it remotely before
        // the switch tail completes lets two CPUs restore and overwrite the
        // same saved stack/context.
        pcb.sched_info().wait_until_not_running();

        let allowed = pi_guard.cpus_allowed.clone();
        let target_cpu = select_task_rq(pcb, prev_cpu, WakeupFlags::WF_TTWU, &allowed);

        if was_uninterruptible || pcb.flags().contains(ProcessFlags::IN_IOWAIT) {
            let prev_rq = cpu_rq(prev_cpu.data() as usize);
            let (prev_rq, _prev_rq_guard) = prev_rq.self_lock();
            if was_uninterruptible {
                prev_rq.dec_nr_uninterruptible();
            }
            if pcb.flags().contains(ProcessFlags::IN_IOWAIT) {
                prev_rq.dec_nr_iowait();
            }
        }

        enqueue_task_on_cpu(pcb, target_cpu, WakeupFlags::WF_TTWU, false);

        Ok(())
    }

    // Complete state write and CPU selection under pi_lock protection.
    pub fn wake_up_new_task(pcb: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        let _guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        debug_assert_eq!(*pcb.sched_info().on_rq.lock_irqsave(), OnRq::None);
        debug_assert!(pcb.sched_info().is_new_task());
        debug_assert!(pcb.sched_info().on_cpu().is_none());

        let pi_guard = pcb.sched_info().pi_lock_irqsave();
        pcb.sched_info().set_state(ProcessState::Runnable);

        let target_cpu = pcb.sched_info().consume_new_task_target_cpu(
            smp_get_processor_id(),
            pi_guard.cpus_allowed.clone(),
            |allowed| {
                let cpu =
                    select_task_rq(pcb, smp_get_processor_id(), WakeupFlags::WF_FORK, allowed);
                if allowed.get(cpu).unwrap_or(false) {
                    Some(cpu)
                } else {
                    None
                }
            },
        )?;

        enqueue_task_on_cpu(pcb, target_cpu, WakeupFlags::WF_FORK, false);

        debug_assert!(!pcb.sched_info().is_new_task());
        Ok(())
    }

    /// Set the specified kernel thread to the SCHED_FIFO scheduling policy.
    ///
    /// task_rq_lock → update_rq_clock → read queued/running →
    /// dequeue/put_prev → modify parameters → enqueue → check_class_changed →
    /// unlock
    pub fn set_fifo_policy(pcb: &Arc<ProcessControlBlock>, prio: i32) -> Result<(), SystemError> {
        if !pcb.flags().contains(ProcessFlags::KTHREAD) {
            return Err(SystemError::EPERM);
        }

        if !(0..crate::sched::prio::MAX_RT_PRIO).contains(&prio) {
            return Err(SystemError::EINVAL);
        }

        let _irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        // Lock ordering: pi_lock → rq_lock, matching Linux task_rq_lock().
        let pi_guard = pcb.sched_info().pi_lock_irqsave();

        let target_cpu = pcb.sched_info().on_cpu().unwrap_or(current_cpu_id());
        let update_clock = target_cpu == smp_get_processor_id();
        let rq = cpu_rq(target_cpu.data() as usize);
        let (rq, rq_guard) = rq.self_lock();
        if update_clock {
            rq.update_rq_clock();
        }

        // Read task state under the lock.
        let old_policy = pcb.sched_info().policy();
        let queued = *pcb.sched_info().on_rq.lock_irqsave() == OnRq::Queued;

        // Determine whether the target is the currently running task on this rq.
        let running = Arc::ptr_eq(&rq.current(), pcb);

        // First dequeue the task from the scheduler, then modify parameters,
        // and finally re-enqueue.
        if queued {
            rq.dequeue_task(
                pcb.clone(),
                DequeueFlag::DEQUEUE_NOCLOCK | DequeueFlag::DEQUEUE_SAVE,
            );
        }

        // A running task must first be put_prev_task to yield its current
        // execution position.
        if running {
            match old_policy {
                SchedPolicy::FIFO => {
                    crate::sched::fifo::FifoScheduler::put_prev_task(rq, pcb.clone())
                }
                SchedPolicy::CFS => {
                    crate::sched::fair::CompletelyFairScheduler::put_prev_task(rq, pcb.clone())
                }
                SchedPolicy::IDLE => {
                    crate::sched::idle::IdleScheduler::put_prev_task(rq, pcb.clone())
                }
                SchedPolicy::RT => todo!("RT scheduler not implemented yet"),
            }
        }

        // Modify scheduling parameters (under rq_lock protection).
        // Matches Linux __setscheduler_params + __setscheduler_prio:
        //   - policy set to FIFO
        //   - prio = normal_prio = MAX_RT_PRIO - 1 - rt_priority (the caller
        //     already passes the kernel prio)
        //   - static_prio is left unchanged (Linux only modifies static_prio for
        //     fair_policy, core.c:7528-7529)
        pcb.sched_info().set_policy(SchedPolicy::FIFO);
        pcb.sched_info().set_prio(prio);
        pcb.sched_info().set_normal_prio(prio);

        // Re-enqueue.
        if queued {
            rq.enqueue_task(
                pcb.clone(),
                EnqueueFlag::ENQUEUE_NOCLOCK | EnqueueFlag::ENQUEUE_RESTORE,
            );
        }

        // Matches Linux __sched_setscheduler: after a running task changes its
        // policy, set_next_task is required.
        if running {
            match pcb.sched_info().policy() {
                SchedPolicy::FIFO => {
                    crate::sched::fifo::FifoScheduler::set_next_task(rq, pcb.clone());
                }
                SchedPolicy::CFS => {
                    crate::sched::fair::CompletelyFairScheduler::set_next_task(rq, pcb.clone());
                }
                _ => {}
            }
        }

        // check_class_changed → preemption check.
        if update_clock {
            rq.check_preempt_current(pcb, WakeupFlags::empty());
        } else {
            rq.check_preempt_remote(pcb, WakeupFlags::empty());
        }

        // Release order: rq_lock first, then pi_lock, matching Linux
        // task_rq_unlock().
        drop(rq_guard);
        drop(pi_guard);

        Ok(())
    }

    /// Wake up a stopped process.
    pub fn wakeup_stop(pcb: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        let _guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let state = pcb.sched_info().state();
        if !state.is_stopped() {
            return if state.is_runnable() {
                Ok(())
            } else {
                Err(SystemError::EINVAL)
            };
        }

        let pi_guard = pcb.sched_info().pi_lock_irqsave();
        let state = pcb.sched_info().state();
        if !state.is_stopped() {
            return if state.is_runnable() {
                Ok(())
            } else {
                Err(SystemError::EINVAL)
            };
        }

        pcb.sched_info().set_state(ProcessState::Runnable);
        fence(Ordering::SeqCst);

        let prev_cpu = pcb.sched_info().on_cpu().unwrap_or(current_cpu_id());
        // A current task may be marked stopped and then resumed before its
        // remote CPU reaches schedule(). It is still queued on prev_cpu in
        // that window, so inspect and wake it under that rq lock; selecting a
        // different rq first would attempt to dequeue it from the wrong queue.
        if *pcb.sched_info().on_rq.lock_irqsave() == OnRq::Queued {
            let rq = cpu_rq(prev_cpu.data() as usize);
            let (rq, _rq_guard) = rq.self_lock();
            if *pcb.sched_info().on_rq.lock_irqsave() == OnRq::Queued {
                let local = prev_cpu == smp_get_processor_id();
                if !Arc::ptr_eq(&rq.current(), pcb) {
                    if local {
                        rq.update_rq_clock();
                        rq.check_preempt_current(pcb, WakeupFlags::WF_TTWU);
                    } else {
                        rq.check_preempt_remote(pcb, WakeupFlags::WF_TTWU);
                    }
                } else if !local {
                    kick_cpu(prev_cpu).ok();
                }
                return Ok(());
            }
        }

        if *pcb.sched_info().on_rq.lock_irqsave() == OnRq::None {
            // An off-rq stopped task retains its previous task_cpu for
            // accounting, but sched_setaffinity may have changed its legal
            // placement while it slept. Select again under pi_lock exactly
            // like ordinary ttwu.
            pcb.sched_info().wait_until_not_running();
            let allowed = pi_guard.cpus_allowed.clone();
            let target_cpu = select_task_rq(pcb, prev_cpu, WakeupFlags::WF_TTWU, &allowed);
            enqueue_task_on_cpu(pcb, target_cpu, WakeupFlags::empty(), false);
        }

        Ok(())
    }

    /// Asynchronously place the target process in the stopped state (used for
    /// job-control stops such as SIGSTOP/SIGTSTP).
    ///
    /// Note: This function marks the **target process** as stopped and does not
    /// need to be called in the target's context. It is the counterpart of
    /// `mark_stop` (which only operates on the current process).
    ///
    /// In Linux, stop is synchronous: the target thread calls
    /// set_special_state(TASK_STOPPED) + schedule() from within its own context
    /// (get_signal() → do_signal_stop()). DragonOS uses an asynchronous approach,
    /// so it must actively dequeue a queued task here to prevent `pick_next_task()`
    /// from briefly running a task with state=Stopped.
    ///
    /// Lock ordering: pi_lock → rq_lock, consistent with wakeup() /
    /// set_fifo_policy() serialization.
    pub fn stop_task(pcb: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        let _guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        // Align with Linux set_special_state(TASK_STOPPED): pi_lock protects the
        // state write and exit check, serializing with concurrent operations also
        // protected by pi_lock in wakeup()/wakeup_stop()/do_exit().
        //
        // In Linux, stop is synchronous: the target thread calls
        // set_special_state(TASK_STOPPED) + schedule() from within
        // get_signal() → do_signal_stop().
        // DragonOS uses an asynchronous approach (stopping the target thread from
        // the sender's context), so it must actively dequeue and kick the remote
        // CPU. This is an architectural difference from Linux, but the lock
        // ordering and dequeue semantics match.
        let pi_guard = pcb.sched_info().pi_lock_irqsave();
        let prev_state = pcb.sched_info().state();
        if prev_state.is_exited() {
            return Err(SystemError::EINTR);
        }
        let target_cpu = pcb.sched_info().on_cpu().unwrap_or_else(current_cpu_id);
        let update_clock = target_cpu == smp_get_processor_id();
        let was_off_rq = *pcb.sched_info().on_rq.lock_irqsave() == OnRq::None;
        let was_uninterruptible = matches!(prev_state, ProcessState::Blocked(false));
        let was_iowait = pcb.flags().contains(ProcessFlags::IN_IOWAIT);

        if was_off_rq && (was_uninterruptible || was_iowait) {
            let prev_rq = cpu_rq(target_cpu.data() as usize);
            let (prev_rq, _prev_rq_guard) = prev_rq.self_lock();
            if was_uninterruptible {
                prev_rq.dec_nr_uninterruptible();
            }
            if was_iowait {
                prev_rq.dec_nr_iowait();
            }
        }

        pcb.sched_info().set_state(ProcessState::Stopped);
        pcb.flags().insert(ProcessFlags::NEED_SCHEDULE);

        let rq = cpu_rq(target_cpu.data() as usize);

        // Lock ordering: pi_lock → rq_lock, consistent with wakeup().
        let (rq, rq_guard) = rq.self_lock();
        if update_clock {
            rq.update_rq_clock();
        }

        let is_current = Arc::ptr_eq(&rq.current(), pcb);

        if *pcb.sched_info().on_rq.lock_irqsave() == OnRq::Queued {
            // Queued and not current → proactively dequeue.
            if !is_current {
                rq.deactivate_task(
                    pcb.clone(),
                    DequeueFlag::DEQUEUE_STOPPED | DequeueFlag::DEQUEUE_NOCLOCK,
                );
            } else if !update_clock {
                // Current task on a remote CPU: only set state + kick; the sender
                // does not dequeue here.
                //
                // Matches Linux signal_wake_up_state() + kick_process():
                //   The sender only sets TIF_SIGPENDING / NEED_SCHEDULE and kicks;
                //   it does not dequeue. The remote CPU's __schedule() handles
                //   the single dequeue when it sees a non-runnable prev.
                //
                // The unified dequeue path in __schedule_inner plus the on_rq
                // guard guarantee idempotency.
                kick_cpu(target_cpu).ok();
            }
        }

        drop(rq_guard);
        drop(pi_guard);
        Ok(())
    }

    /// Mark the current process as perpetually sleeping. The caller is
    /// responsible for subsequently triggering a reschedule.
    ///
    /// ## Note
    ///
    /// - The caller must not hold the sched_info lock before entering this
    ///   function.
    /// - Interrupts must be disabled before entering this function.
    /// - After entering this function, the caller must ensure logical
    ///   correctness to prevent the task from being re-added to the run queue.
    pub fn mark_sleep(interruptable: bool) -> Result<(), SystemError> {
        assert!(
            !CurrentIrqArch::is_irq_enabled(),
            "interrupt must be disabled before enter ProcessManager::mark_sleep()"
        );
        let pcb = ProcessManager::current_pcb();
        if !pcb.sched_info().state().is_exited() {
            pcb.sched_info()
                .set_state(ProcessState::Blocked(interruptable));
            pcb.flags().insert(ProcessFlags::NEED_SCHEDULE);
            fence(Ordering::SeqCst);
            return Ok(());
        }
        return Err(SystemError::EINTR);
    }

    /// Mark the current process as stopped. The caller is responsible for
    /// subsequently triggering a reschedule.
    ///
    /// ## Note
    ///
    /// - The caller must not hold the sched_info lock before entering this
    ///   function.
    /// - Interrupts must be disabled before entering this function.
    pub fn mark_stop() -> Result<(), SystemError> {
        assert!(
            !CurrentIrqArch::is_irq_enabled(),
            "interrupt must be disabled before enter ProcessManager::mark_stop()"
        );

        let pcb = ProcessManager::current_pcb();
        if !pcb.sched_info().state().is_exited() {
            // pi_lock protects the STOPPED write, serializing with concurrent
            // wakeup()/wakeup_stop().
            let _pi_guard = pcb.sched_info().pi_lock_irqsave();
            pcb.sched_info().set_state(ProcessState::Stopped);
            pcb.flags().insert(ProcessFlags::NEED_SCHEDULE);
            return Ok(());
        }
        return Err(SystemError::EINTR);
    }
}
