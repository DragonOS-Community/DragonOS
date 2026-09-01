use core::sync::atomic::{fence, Ordering};

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::{cpu::current_cpu_id, CurrentIrqArch},
    exception::InterruptArch,
    libs::cpumask::CpuMask,
    process::{ProcessControlBlock, ProcessFlags, ProcessManager, ProcessState},
    sched::{
        cpu_rq, enqueue_task_on_cpu, select_task_rq, DequeueFlag, EnqueueFlag, LinuxSchedPolicy,
        OnRq, SchedClass, WakeupFlags,
    },
    smp::{core::smp_get_processor_id, kick_cpu},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchedChangeRequest {
    #[allow(dead_code)]
    Normal {
        reset_on_fork: bool,
    },
    Fifo {
        priority: i32,
    },
}

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
        let starts_stopped = pcb.sched_info().state().is_stopped();

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

        // A CLONE_THREAD child published into an existing group-stop consumes
        // its one-time placement state but must not become runnable before
        // SIGCONT. wakeup_stop() will select and enqueue it normally later.
        if starts_stopped {
            debug_assert!(!pcb.sched_info().is_new_task());
            return Ok(());
        }

        pcb.sched_info().set_state(ProcessState::Runnable);

        enqueue_task_on_cpu(pcb, target_cpu, WakeupFlags::WF_FORK, false);

        debug_assert!(!pcb.sched_info().is_new_task());
        Ok(())
    }

    /// Atomically change the base scheduler parameters of an already placed task.
    pub(crate) fn set_scheduler(
        pcb: &Arc<ProcessControlBlock>,
        request: SchedChangeRequest,
    ) -> Result<(), SystemError> {
        if let SchedChangeRequest::Fifo { priority } = request {
            if !(0..crate::sched::prio::MAX_RT_PRIO - 1).contains(&priority) {
                return Err(SystemError::EINVAL);
            }
        }

        let _irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        loop {
            // Lock ordering matches Linux task_rq_lock(): pi_lock -> rq_lock.
            let mut pi_guard = pcb.sched_info().pi_lock_irqsave();
            let old_class = pcb.sched_info().sched_class();
            if old_class == SchedClass::Idle {
                return Err(SystemError::EINVAL);
            }

            // Published tasks retain their last rq while sleeping or exiting.
            // A task without a CPU can have a Fair entity bound to another rq,
            // so this PR deliberately rejects that unplaced state.
            let Some(target_cpu) = pcb.sched_info().on_cpu() else {
                return Err(SystemError::EINVAL);
            };
            let rq = cpu_rq(target_cpu.data() as usize);
            let (rq, rq_guard) = rq.self_lock();

            let stable = pcb.sched_info().on_cpu() == Some(target_cpu)
                && *pcb.sched_info().on_rq.lock_irqsave() != OnRq::Migrating;
            if !stable {
                drop(rq_guard);
                drop(pi_guard);
                core::hint::spin_loop();
                continue;
            }

            rq.update_rq_clock();

            let old_policy = pcb.sched_info().policy();
            let old_prio = pcb.sched_info().prio();
            let old_reset = pi_guard.sched_reset_on_fork();
            let (new_policy, new_prio, new_reset) = match request {
                SchedChangeRequest::Normal { reset_on_fork } => (
                    LinuxSchedPolicy::Normal,
                    pcb.sched_info().static_prio(),
                    reset_on_fork,
                ),
                SchedChangeRequest::Fifo { priority } => (LinuxSchedPolicy::Fifo, priority, false),
            };
            let new_class = new_policy.base_sched_class();

            if old_policy == new_policy && old_prio == new_prio && old_reset == new_reset {
                drop(rq_guard);
                drop(pi_guard);
                return Ok(());
            }

            let queued = *pcb.sched_info().on_rq.lock_irqsave() == OnRq::Queued;
            let running = Arc::ptr_eq(&rq.current(), pcb);
            // The final switch-out removes a dead Fair entity's PELT
            // contribution. Linux still permits scheduler parameter updates
            // for dead tasks, but such an off-rq task must not be attached
            // again after its task-dead accounting has completed.
            let account_class_change = !pcb.sched_info().state().is_exited() || queued || running;

            if queued {
                rq.dequeue_task(
                    pcb.clone(),
                    DequeueFlag::DEQUEUE_SAVE
                        | DequeueFlag::DEQUEUE_MOVE
                        | DequeueFlag::DEQUEUE_NOCLOCK,
                );
            }
            if running {
                rq.put_prev_task_for_class(old_class, pcb.clone());
            }
            if account_class_change
                && old_class == SchedClass::Fair
                && new_class != SchedClass::Fair
            {
                crate::sched::fair::CompletelyFairScheduler::switched_from_fair(rq, pcb);
            }

            pcb.sched_info().set_policy(new_policy);
            pcb.sched_info().set_prio(new_prio);
            pcb.sched_info().set_normal_prio(new_prio);
            pi_guard.set_sched_reset_on_fork(new_reset);

            if account_class_change
                && old_class != SchedClass::Fair
                && new_class == SchedClass::Fair
            {
                crate::sched::fair::CompletelyFairScheduler::switched_to_fair(rq, pcb);
            }

            if queued {
                let mut flags = EnqueueFlag::ENQUEUE_RESTORE
                    | EnqueueFlag::ENQUEUE_MOVE
                    | EnqueueFlag::ENQUEUE_NOCLOCK;
                if old_prio < new_prio {
                    flags |= EnqueueFlag::ENQUEUE_HEAD;
                }
                rq.enqueue_task(pcb.clone(), flags);
            }
            if running {
                rq.set_next_task_for_class(new_class, pcb.clone());
            }

            rq.check_scheduler_changed(pcb, old_class, old_prio);

            drop(rq_guard);
            drop(pi_guard);
            return Ok(());
        }
    }

    /// Set a trusted kernel thread to the SCHED_FIFO policy.
    pub fn set_fifo_policy(pcb: &Arc<ProcessControlBlock>, prio: i32) -> Result<(), SystemError> {
        if !pcb.flags().contains(ProcessFlags::KTHREAD) {
            return Err(SystemError::EPERM);
        }

        Self::set_scheduler(pcb, SchedChangeRequest::Fifo { priority: prio })
    }

    /// Publish a validated affinity mask and apply any required migration.
    pub(crate) fn set_cpus_allowed(
        pcb: &Arc<ProcessControlBlock>,
        mask: CpuMask,
    ) -> Result<(), SystemError> {
        let mut pi_guard = pcb.sched_info().pi_lock_irqsave();
        pi_guard.set_cpus_allowed(mask.clone());

        if pcb.sched_info().is_new_task() {
            return Ok(());
        }

        if pcb
            .sched_info()
            .migrate_to()
            .is_some_and(|cpu| !mask.get(cpu).unwrap_or(false))
        {
            pcb.sched_info().set_migrate_to(None);
            pcb.flags().remove(ProcessFlags::NEED_MIGRATE);
        }

        if let Some(cpu) = pcb.sched_info().on_cpu() {
            if !mask.get(cpu).unwrap_or(false) {
                let dest_cpu = select_task_rq(pcb, cpu, WakeupFlags::WF_TTWU, &mask);
                crate::sched::request_task_migration(pcb, dest_cpu)?;
            }
        }

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

        Self::wakeup_stop_locked(pcb)
    }

    /// Wake a stopped task while the caller keeps the ptrace relation
    /// transaction locked.
    ///
    /// This narrow entry point lets ptrace teardown validate the old session,
    /// clear its active stop and commit `Stopped -> Runnable` without an
    /// unlock/re-attach window.  The caller's lock order is
    /// `PTRACE_RELATION_LOCK -> sighand -> pi_lock -> rq_lock`; this function
    /// never acquires the relation or sighand locks in the reverse direction.
    pub(crate) fn wakeup_stop_relation_locked(
        pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        let _guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        Self::wakeup_stop_locked(pcb)
    }

    fn wakeup_stop_locked(pcb: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
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
            let target_cpu =
                select_task_rq(pcb, prev_cpu, WakeupFlags::WF_TTWU, &pi_guard.cpus_allowed);
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
        let _pi_guard = pcb.sched_info().pi_lock_irqsave();
        let state = pcb.sched_info().state();
        if state.is_exited() {
            return Err(SystemError::EINTR);
        }
        if state.is_stopped() {
            return Ok(());
        }
        pcb.sched_info()
            .set_state(ProcessState::Blocked(interruptable));
        pcb.flags().insert(ProcessFlags::NEED_SCHEDULE);
        fence(Ordering::SeqCst);
        return Ok(());
    }

    /// Undo a just-completed Self::mark_sleep: handles a wakeup arriving between prepare_sleep and mark_sleep.
    pub fn undo_mark_sleep() {
        let pcb = ProcessManager::current_pcb();
        let _pi_guard = pcb.sched_info().pi_lock_irqsave();
        let state = pcb.sched_info().state();
        if state.is_stopped() {
            // Preserve Stopped and the NEED_SCHEDULE written by the stop.
            return;
        }
        if state.is_blocked() {
            // Only promote the Blocked written by this mark_sleep back to Runnable.
            pcb.sched_info().set_state(ProcessState::Runnable);
            fence(Ordering::SeqCst);
        }
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
