use core::{
    hint::spin_loop,
    sync::atomic::{compiler_fence, Ordering},
};

use alloc::sync::Arc;
use log::{error, warn};

use super::ALL_PROCESS;
use crate::{
    arch::{
        ipc::signal::{SigFlags, Signal},
        CurrentIrqArch,
    },
    driver::tty::tty_job_control::TtyJobCtrlManager,
    exception::InterruptArch,
    ipc::sighand::{NaturalParentNotifyToken, ReapTransition},
    ipc::signal_types::{SigCode, SigInfo, SigType},
    libs::futex::{
        constant::{FutexFlag, FUTEX_BITSET_MATCH_ANY},
        futex::{Futex, RobustListHead},
    },
    mm::IDLE_PROCESS_ADDRESS_SPACE,
    process::{
        kthread::KernelThreadMechanism,
        pid::{Pid, PidType},
        ptrace, ProcessControlBlock, ProcessFlags, ProcessManager, ProcessState, RawPid,
    },
    sched::{SchedMode, __schedule_with_current},
    smp::core::smp_get_processor_id,
    syscall::user_access::clear_user_protected,
};

impl ProcessManager {
    /// Notify the parent process after a child process exits.
    #[inline(never)]
    fn exit_notify(current: &Arc<ProcessControlBlock>) {
        let sighand = current.sighand();
        let claimed_exec_leader = sighand.claim_group_exec_leader_exit(current);

        // A claimed old leader must retain all hashed identity until the exec
        // owner swaps it. In particular it is never autoreaped here.
        if claimed_exec_leader {
            current.set_exit_state_zombie();
            Self::wake_pidfd_pollers_for_task_exit(current);
            Self::notify_ptrace_parent(
                ptrace::ptracer_of(current).map(|tracer| (tracer, Signal::SIGCHLD as i32)),
            );
            current.mark_exit_notify_complete();
            let completed = sighand.complete_group_exec_leader_exit(current);
            debug_assert!(completed);
            return;
        }

        // Have the INIT process adopt all children.
        if current.raw_pid() != RawPid(1) {
            unsafe {
                current
                    .adopt_childen()
                    .unwrap_or_else(|e| panic!("adopte_childen failed: error: {e:?}"))
            };
            ProcessManager::exit_ptrace(current);
            let (ptrace_notification, natural_token, autoreap_nonleader) = {
                // Keep ptrace classification stable until Zombie publication
                // and natural-parent ownership are committed. Concurrent
                // detach wakes the natural parent after this snapshot, so it
                // cannot lose the transition either.
                let _relation_guard = crate::process::PTRACE_RELATION_LOCK.lock_irqsave();
                let tracer = ptrace::ptracer_of_locked(current);
                let leader = current.is_thread_group_leader();
                let (group_empty, natural_token) = if leader {
                    // Publish Zombie and inspect group emptiness under the same
                    // SigHand lock used by the last-sibling remove+claim path.
                    // Whichever side observes the group become empty therefore
                    // owns the one natural-parent notification transaction.
                    let (empty, token) =
                        sighand.try_claim_natural_parent_notify_with(current, || {
                            current.set_exit_state_zombie();
                            let empty = !current
                                .threads_read_irqsave()
                                .group_tasks
                                .iter()
                                .any(|task| task.upgrade().is_some());
                            (empty, tracer.is_none() && empty)
                        });
                    (empty, token)
                } else {
                    current.set_exit_state_zombie();
                    (false, None)
                };
                let autoreap_nonleader = tracer.is_none() && !leader;
                let ptrace_notification = tracer.map(|tracer| {
                    let natural_tracer = current
                        .real_parent_pcb()
                        .map(|parent| Arc::ptr_eq(&parent, &tracer))
                        .unwrap_or(false);
                    let signal = if leader && group_empty && natural_tracer {
                        current.exit_signal.load(Ordering::Acquire)
                    } else {
                        Signal::SIGCHLD as i32
                    };
                    (tracer, signal)
                });
                (ptrace_notification, natural_token, autoreap_nonleader)
            };
            // Ordinary exit does not become wait-visible until adoption and
            // ptrace teardown have stopped using the old identity. For a
            // group-empty leader, notification Pending is claimed first so a
            // polling parent cannot consume Zombie before the notifier owns
            // the Done-before-wake transaction.
            Self::wake_pidfd_pollers_for_task_exit(current);
            Self::notify_ptrace_parent(ptrace_notification);

            if autoreap_nonleader {
                // Linux autoreaps ordinary nonleaders. release/__unhash_process
                // completes the generation token only after identity teardown.
                if current.try_mark_dead_from_zombie() {
                    unsafe { ProcessManager::release(current.raw_pid()) };
                }
            } else if let Some(token) = natural_token {
                Self::notify_natural_parent_owned(current, token);
            }
        }

        current.mark_exit_notify_complete();
        sighand.complete_group_exec_leader_exit(current);
    }

    /// Return the stable thread-group leader and whether `current`, which has
    /// already published EXITING, is the last live task in the group.
    ///
    /// The caller must hold PID_MEMBERSHIP_LOCK across both mark_exiting() and
    /// this scan, so concurrent exits cannot both miss the unique last task.
    fn thread_group_last_live_task(
        current: &Arc<ProcessControlBlock>,
    ) -> (Arc<ProcessControlBlock>, bool) {
        let leader = current
            .threads_read_irqsave()
            .group_leader()
            .unwrap_or_else(|| current.clone());
        let leader_threads = leader.threads_read_irqsave();
        let is_live = |task: &Arc<ProcessControlBlock>| {
            !task.flags().contains(ProcessFlags::EXITING)
                && !task.is_exited()
                && !task.is_zombie()
                && !task.is_dead()
        };

        if !Arc::ptr_eq(&leader, current) && is_live(&leader) {
            return (leader.clone(), false);
        }
        for weak in &leader_threads.group_tasks {
            let Some(task) = weak.upgrade() else {
                continue;
            };
            if !Arc::ptr_eq(&task, current) && is_live(&task) {
                return (leader.clone(), false);
            }
        }
        drop(leader_threads);
        (leader, true)
    }

    fn notify_ptrace_parent(tracer: Option<(Arc<ProcessControlBlock>, i32)>) {
        if let Some((tracer, signal)) = tracer {
            if signal > 0 {
                let _ = crate::ipc::kill::send_signal_to_pcb(tracer.clone(), Signal::from(signal));
            }
            ProcessManager::wake_wait_parent(&tracer);
        }
    }

    /// Complete the unique natural-parent notification transaction. Signal
    /// delivery and an optional owner-authorized autoreap happen while the
    /// phase is Pending; Done is then published before the final parent wake.
    pub(crate) fn notify_natural_parent_owned(
        child: &Arc<ProcessControlBlock>,
        token: NaturalParentNotifyToken,
    ) {
        let Some(parent) = child.real_parent_pcb() else {
            child.sighand().complete_natural_parent_notify(token);
            return;
        };
        let exit_signal = child.exit_signal.load(Ordering::SeqCst);
        let disposition = parent.sighand().handler(Signal::SIGCHLD);
        let ignored = disposition
            .as_ref()
            .map(|sa| sa.is_ignore())
            .unwrap_or(false);
        let no_cldwait = disposition
            .as_ref()
            .map(|sa| sa.flags().contains(SigFlags::SA_NOCLDWAIT))
            .unwrap_or(false);
        let autoreap = exit_signal == Signal::SIGCHLD as i32 && (ignored || no_cldwait);

        if exit_signal > 0 && !(autoreap && ignored) {
            if let Err(e) =
                crate::ipc::kill::send_signal_to_pcb(parent.clone(), Signal::from(exit_signal))
            {
                warn!(
                    "failed to send exit signal for {:?} to parent {:?}: {:?}",
                    child.raw_pid(),
                    parent.raw_pid(),
                    e
                );
            }
        }

        if autoreap
            && child
                .sighand()
                .try_reap_natural_child_as_notify_owner(child, &token)
                == ReapTransition::Reaped
        {
            unsafe { ProcessManager::release(child.raw_pid()) };
        }

        assert!(
            child.sighand().complete_natural_parent_notify(token),
            "natural-parent notification ownership changed"
        );
        ProcessManager::wake_wait_parent(&parent);

        if child.is_kthread() {
            KernelThreadMechanism::notify_daemon();
        }
    }

    fn wake_pidfd_pollers_for_task_exit(task: &Arc<ProcessControlBlock>) {
        let thread_pid = task.pid();
        thread_pid.wake_pidfd_pollers();
        if let Some(tgid_pid) = task.task_pid_ptr(PidType::TGID) {
            if !Arc::ptr_eq(&thread_pid, &tgid_pid) {
                tgid_pid.wake_pidfd_pollers();
            }
        }
    }

    /// Exit the current process.
    ///
    /// ## Parameters
    ///
    /// - `exit_code`: The process exit code.
    ///
    /// ## Note
    /// For a normally-exiting process, the status code should be shifted left by
    /// 8 bits so that userspace can read the exit code correctly. For a process
    /// terminated by a signal, the status code is the signal number in the low 7
    /// bits and does not need shifting.
    ///
    /// Therefore, the caller must ensure that the passed `exit_code` has already
    /// been shifted as appropriate.
    pub fn exit(exit_code: usize) -> ! {
        // Check if the init process is attempting to exit; panic if so.
        let current_pcb = ProcessManager::current_pcb();

        if current_pcb.raw_pid() == RawPid(0) {
            log::error!(
                "Idle process (pid=0) attempted to exit with code {}. Halting current cpu.",
                exit_code
            );
            loop {
                spin_loop();
            }
        }

        if current_pcb.raw_pid() == RawPid(1) {
            log::error!(
                "Init process (pid=1) attempted to exit with code {}. This should not happen and indicates a serious system error.",
                exit_code
            );
            loop {
                spin_loop();
            }
        }

        let pid: Arc<Pid>;
        let raw_pid = current_pcb.raw_pid();
        // log::debug!("[exit: {}]", raw_pid.data());
        {
            let pcb = current_pcb.clone();
            let (group_leader, group_dead) = {
                let _membership_guard = crate::process::pid::pid_membership_lock();
                pcb.mark_exiting();
                Self::thread_group_last_live_task(&pcb)
            };
            pid = pcb.pid();
            if pid.is_child_reaper() {
                pid.ns_of_pid().disable_pid_allocation();
            }
            pcb.wait_queue.mark_dead();

            // Perform post-exit work for the process.
            let thread = pcb.thread.write_irqsave();
            let clear_child_tid = thread.clear_child_tid;
            let vfork_done = thread.vfork_done.clone();
            drop(thread);
            if let Some(addr) = clear_child_tid {
                // Per Linux semantics: first clear *clear_child_tid in userspace,
                // then futex_wake(addr).
                let cleared_ok = unsafe {
                    match clear_user_protected(addr, core::mem::size_of::<i32>()) {
                        Ok(_) => true,
                        Err(e) => {
                            // The clear_child_tid pointer may be invalid or
                            // unmapped: do not panic because of this.
                            warn!("clear tid failed: {e:?}");
                            false
                        }
                    }
                };
                // If *clear_child_tid cannot be cleared, avoid futex_wake as well
                // (avoid further invalid userspace accesses).
                if cleared_ok
                    && Arc::strong_count(&pcb.basic().user_vm().expect("User VM Not found")) > 1
                {
                    // Linux uses the FUTEX_SHARED flag to wake clear_child_tid.
                    // This allows cross-process/thread synchronization (e.g.
                    // pthread_join).
                    let _ =
                        Futex::futex_wake(addr, FutexFlag::FLAGS_SHARED, 1, FUTEX_BITSET_MATCH_ANY);
                }
            }
            compiler_fence(Ordering::SeqCst);

            RobustListHead::exit_robust_list(pcb.clone());
            // If this process was created via vfork, complete the completion.
            if let Some(vd) = vfork_done {
                vd.complete_all();
            }

            // Linux exit_mm() happens before exit_files(): after clear_child_tid
            // and robust-list cleanup have consumed user memory, the exiting task
            // must stop exposing a user-visible mm even if file close blocks.
            //
            // The CPU-visible active mm is switched to idle here and tracked by
            // per-CPU TlbState. The PCB-visible user_vm becomes None, matching
            // Linux task->mm == NULL after exit_mm().
            let old_user_vm = pcb.with_task_lock_irqsave(|| {
                let idle_vm = IDLE_PROCESS_ADDRESS_SPACE();
                let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
                let cpu = smp_get_processor_id();
                let mut basic = pcb.basic_mut();
                let old_vm = unsafe { basic.replace_user_vm(None) };
                idle_vm.active_cpus_set(cpu);
                unsafe { idle_vm.make_current() };
                if let Some(old_vm) = old_vm.as_ref() {
                    old_vm.active_cpus_clear(cpu);
                }
                unsafe { crate::mm::tlb::tlb_state_set_loaded_mm(idle_vm) };
                drop(basic);
                drop(irq_guard);
                old_vm
            });
            if let Some(old_vm) = old_user_vm.as_ref() {
                let last_user = !Self::mm_has_user_tasks(old_vm);
                if last_user {
                    unsafe {
                        old_vm.write().unmap_all();
                    }
                    crate::mm::oom::note_oom_victim_mm_released(old_vm.id());
                }
            }
            drop(old_user_vm);

            pcb.exit_files();
            // Linux exit_fs() follows exit_files(). A zombie must not keep
            // cwd/root path references alive, otherwise an already-reaped
            // chrooted child can make an unrelated umount report EBUSY.
            pcb.exit_fs();
            pcb.exit_timers();

            if group_dead {
                let (current_tty, is_session_leader, sid) = {
                    let siginfo = group_leader.sig_info_irqsave();
                    (siginfo.tty(), siginfo.is_session_leader, pcb.task_session())
                };
                if let Some(tty) = current_tty {
                    if is_session_leader {
                        let tty_pgrp = sid.as_ref().and_then(|sid| {
                            TtyJobCtrlManager::remove_session_tty_if_owner(&tty, sid)
                        });
                        if let Some(pgrp) = tty_pgrp {
                            let _ = crate::ipc::kill::send_signal_to_pgid(&pgrp, Signal::SIGHUP);
                        }
                    } else {
                        group_leader.sig_info_mut().set_tty(None);
                    }
                } else {
                    group_leader.sig_info_mut().set_tty(None);
                }
            }

            // Linux semantics: a zombie must not appear in cgroup.procs.
            // Hold cgroup_accounting_lock to avoid deadlock with cgroup.procs writes.
            {
                let _cgroup_guard = crate::cgroup::cgroup_accounting_lock().lock();
                pcb.task_cgroup_node().remove_task(raw_pid);
            }
            if pcb.is_kthread() {
                let exited_completion = {
                    let worker_private = pcb.worker_private();
                    worker_private
                        .as_ref()
                        .and_then(|x| x.kernel_thread())
                        .map(|x| x.exited_completion())
                };
                if let Some(exited_completion) = exited_completion {
                    exited_completion.complete_all();
                }
            }

            // Align with Linux do_task_dead(): do not manually deactivate_task
            // here. The task remains on the rq; later __schedule() will detect
            // the Exited state and perform the single deactivate_task, avoiding
            // a double dequeue that would underflow nr_running.

            let _final_irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
            // Set the scheduling state to Exited before calling exit_notify at the
            // very end.
            // Mirrors Linux do_task_dead()'s set_special_state(TASK_DEAD):
            // complete the state write under pi_lock protection, serializing with
            // concurrent wakeups also protected by pi_lock in wakeup().
            {
                let _pi_guard = pcb.sched_info.pi_lock_irqsave();
                pcb.sched_info.set_state(ProcessState::Exited(exit_code));
            }
            ProcessManager::exit_notify(&pcb);

            __schedule_with_current(SchedMode::SM_NONE, current_pcb);
            error!("raw_pid {raw_pid:?} exited but sched again!");
            #[allow(clippy::empty_loop)]
            loop {
                spin_loop();
            }
        }
    }

    /// Exit the entire thread group (mirroring Linux do_group_exit semantics).
    ///
    /// - `exit_code`:
    ///   - For sys_exit_group: should already be shifted left by 8 bits (wstatus
    ///     encoding).
    ///   - For fatal signals: the low 7 bits hold the signal number; no shift is
    ///     needed.
    pub fn group_exit(exit_code: usize) -> ! {
        let final_exit_code: usize;

        // Use a closure to ensure all pointers held here are dropped.
        {
            // Check if this is the init process.
            let current_pcb = ProcessManager::current_pcb();

            if current_pcb.raw_pid() == RawPid(1) {
                log::error!(
                "Init process (pid=1) attempted to group_exit with code {}. This should not happen and indicates a serious system error.",
                exit_code
            );
                loop {
                    spin_loop();
                }
            }

            // 1. Atomically set the GROUP_EXIT flag and group_exit_code on the
            //    shared sighand. If another thread has already set it, reuse
            //    the existing exit code.
            let sighand = current_pcb.sighand();
            final_exit_code = sighand.start_group_exit(exit_code);

            // 2. Send SIGKILL to other threads in the same thread group,
            //    waking and forcefully terminating them. Mirroring Linux
            //    zap_other_threads semantics, this only delivers SIGKILL; the
            //    actual exit happens in each thread's own context.
            {
                let send_sigkill_thread = |task: Arc<ProcessControlBlock>| {
                    if task.flags().contains(ProcessFlags::EXITING) {
                        return;
                    }
                    let mut info = SigInfo::new(
                        Signal::SIGKILL,
                        0,
                        SigCode::Kernel,
                        SigType::Kill {
                            pid: RawPid::new(0),
                            uid: 0,
                        },
                    );
                    let _ = Signal::SIGKILL.send_signal_info_to_pcb(
                        Some(&mut info),
                        task,
                        PidType::PID,
                    );
                };

                // Obtain the full thread list from the thread group leader's
                // ThreadInfo to avoid missing threads when viewed from a
                // non-leader thread (where group_tasks may be empty).
                let leader = {
                    let ti = current_pcb.thread.read_irqsave();
                    ti.group_leader().unwrap_or_else(|| current_pcb.clone())
                };

                let group_tasks = {
                    let ti = leader.threads_read_irqsave();
                    ti.group_tasks.clone()
                };

                // First, send SIGKILL to the group leader (if the current
                // thread is not the leader).
                if !Arc::ptr_eq(&leader, &current_pcb)
                    && !leader.flags().contains(ProcessFlags::EXITING)
                {
                    send_sigkill_thread(leader.clone());
                }

                // Then iterate over the group_tasks maintained by the leader
                // and send SIGKILL to other threads.
                for weak in group_tasks {
                    if let Some(task) = weak.upgrade() {
                        // Skip the current thread itself.
                        if task.raw_pid() == current_pcb.raw_pid() {
                            continue;
                        }
                        if task.flags().contains(ProcessFlags::EXITING) {
                            continue;
                        }
                        send_sigkill_thread(task);
                    }
                }
            }

            drop(current_pcb);
        }
        // 3. The current thread proceeds with the normal exit flow using the
        //    unified exit code.

        ProcessManager::exit(final_exit_code);
    }

    /// Remove a process from the global process list and from the parent's
    /// children list.
    ///
    /// # Parameters
    ///
    /// - `pid`: The **global** pid of the process.
    ///
    /// # Note
    ///
    /// The caller **must not hold** the parent's children lock before calling
    /// this function, otherwise a deadlock will occur.
    pub(crate) unsafe fn release(pid: RawPid) {
        let pcb = ProcessManager::find(pid);
        if let Some(ref pcb) = pcb {
            ProcessManager::exit_ptrace(pcb);
            ProcessManager::ptrace_unlink_tracee(pcb);

            let parent_child_vpid = pcb.real_parent_pcb().and_then(|parent| {
                // A concurrently exiting parent may already have unhashed its
                // PID before this nonleader is autoreaped. Its children list
                // has then been consumed by the adoption transaction, so
                // there is no attached namespace/list entry left to clean.
                let parent_ns = parent
                    .task_pid_ptr(PidType::PID)
                    .and_then(|pid| pid.try_ns_of_pid())?;
                pcb.task_pid_nr_ns(PidType::PID, Some(parent_ns))
                    .map(|vpid| (parent, vpid))
            });

            // Remove from the parent's children list.
            if let Some((parent, vpid)) = parent_child_vpid {
                let mut children = parent.children.write();
                children.retain(|&p| p != vpid);
            }

            // Revoke the old PCB's global numeric lookup before __exit_signal()
            // can detach the final PID link and return that number to the
            // allocator. Otherwise a concurrent fork may publish a new task at
            // the same key and this release would delete the new task instead.
            ALL_PROCESS.lock_irqsave().as_mut().unwrap().remove(&pid);

            pcb.__exit_signal();
            {
                let _cgroup_guard = crate::cgroup::cgroup_accounting_lock().lock();
                pcb.task_cgroup_node().uncharge_pids(1);
            }
        }
    }

    pub fn ptrace_unlink_tracee(tracee: &Arc<ProcessControlBlock>) {
        ptrace::unlink_tracee(tracee)
    }

    pub fn exit_ptrace(tracer: &Arc<ProcessControlBlock>) {
        ptrace::exit_ptrace(tracer)
    }

    /// Wake waiters that can observe child state changes through `parent`.
    ///
    /// Linux uses the thread-group shared `signal->wait_chldexit` queue.
    /// DragonOS currently has one `wait_queue` per task, while `do_wait()`
    /// sleeps on the caller's thread-group leader. Any path that publishes a
    /// wait-visible transition through a parent relation must therefore wake
    /// both the concrete parent task and its thread-group leader.
    pub(crate) fn wake_wait_parent(parent: &Arc<ProcessControlBlock>) {
        parent
            .wait_queue
            .wakeup_all(Some(ProcessState::Blocked(true)));

        let parent_group_leader = {
            let ti = parent.thread.read_irqsave();
            ti.group_leader()
        };
        if let Some(leader) = parent_group_leader {
            if !Arc::ptr_eq(&leader, parent) {
                leader
                    .wait_queue
                    .wakeup_all(Some(ProcessState::Blocked(true)));
            }
        }
    }
}
