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
    ipc::signal_types::{SigCode, SigInfo, SigType, SignalFlags},
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
        let exec_task = if sighand.flags_contains(SignalFlags::GROUP_EXEC) {
            sighand.group_exec_task()
        } else {
            None
        };
        let is_mt_exec_leader = current.is_thread_group_leader()
            && exec_task
                .as_ref()
                .map(|t| !Arc::ptr_eq(t, current))
                .unwrap_or(false);
        if sighand.flags_contains(SignalFlags::GROUP_EXEC) {
            if let Some(exec_task) = exec_task.as_ref() {
                if !Arc::ptr_eq(exec_task, current) {
                    let notify_count = sighand.group_exec_notify_count();
                    if notify_count < 0 {
                        // mt-exec: the exec thread is waiting for the leader to exit
                        sighand.wake_group_exec_waiters();
                    } else if !current.is_thread_group_leader() {
                        sighand.dec_group_exec_notify_count_and_wake();
                    }
                }
            }
            let should_clear = exec_task
                .as_ref()
                .map(|t| Arc::ptr_eq(t, current))
                .unwrap_or(false);
            if should_clear {
                sighand.finish_group_exec();
            }
        }
        // mt-exec: when the leader exits, only mark it zombie to avoid
        // triggering the normal exit notification/adoption path.
        if is_mt_exec_leader {
            current.set_exit_state_zombie();
            Self::wake_pidfd_pollers_for_task_exit(current);
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
            // Mark as Zombie before notifying the parent so that wait is
            // guaranteed to see the state change.
            current.set_exit_state_zombie();
            Self::wake_pidfd_pollers_for_task_exit(current);
            let r = current.parent_pcb.read_irqsave().upgrade();
            if r.is_none() {
                return;
            }
            let parent_pcb = r.unwrap();

            // Check the child's exit_signal; send a notification signal only when
            // the signal number is positive.
            // Linux semantics: exit_signal=0 means no signal sent but still waitable,
            // -1 means a non-leader thread.
            let exit_signal = current.exit_signal.load(Ordering::SeqCst);
            let sigchld_disposition = parent_pcb.sighand().handler(Signal::SIGCHLD);
            let sigchld_ignored = sigchld_disposition
                .as_ref()
                .map(|sa| sa.is_ignore())
                .unwrap_or(false);
            let sigchld_no_cldwait = sigchld_disposition
                .as_ref()
                .map(|sa| sa.flags().contains(SigFlags::SA_NOCLDWAIT))
                .unwrap_or(false);
            let autoreap = !current.is_ptraced()
                && exit_signal == Signal::SIGCHLD as i32
                && (sigchld_ignored || sigchld_no_cldwait);
            let is_kthread = current.is_kthread();
            if autoreap && current.try_mark_dead_from_zombie() {
                unsafe { ProcessManager::release(current.raw_pid()) };
            }

            if exit_signal > 0 && !(autoreap && sigchld_ignored) {
                let r = crate::ipc::kill::send_signal_to_pcb(
                    parent_pcb.clone(),
                    Signal::from(exit_signal),
                );
                if let Err(e) = r {
                    warn!(
                        "failed to send kill signal to {:?}'s parent pcb {:?}: {:?}",
                        current.raw_pid(),
                        parent_pcb.raw_pid(),
                        e
                    );
                }
            }

            // Wake the wait parent regardless of exit_signal value.
            // exit_signal only determines which signal to send, not whether to wake wait.
            // DragonOS waiters sleep on per-task wait_queue, so we must also wake
            // the parent's thread-group leader to compensate for Linux's shared
            // signal->wait_chldexit queue semantics.
            ProcessManager::wake_wait_parent(&parent_pcb);

            // Explicitly wake kthreadd when a kthread exits so that it can
            // reap the zombie.
            if is_kthread {
                KernelThreadMechanism::notify_daemon();
            }

            // TODO: The signal delivery decision should also consider thread-group
            // information.
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
            pcb.mark_exiting();
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

            let (current_tty, is_session_leader, sid) = {
                let siginfo = pcb.sig_info_irqsave();
                (siginfo.tty(), siginfo.is_session_leader, pcb.task_session())
            };
            if let Some(tty) = current_tty {
                if is_session_leader {
                    let tty_pgrp = tty.core().contorl_info_irqsave().pgid.clone();
                    if let Some(pgrp) = tty_pgrp {
                        let _ = crate::ipc::kill::send_signal_to_pgid(&pgrp, Signal::SIGHUP);
                    }
                    TtyJobCtrlManager::remove_session_tty(&tty);
                    if let Some(sid) = sid {
                        TtyJobCtrlManager::session_clear_tty(sid);
                    } else {
                        pcb.sig_info_mut().set_tty(None);
                    }
                } else {
                    let mut g = tty.core().contorl_info_irqsave();
                    if g.pgid == Some(pid) {
                        g.pgid = None;
                    }
                    drop(g);
                    pcb.sig_info_mut().set_tty(None);
                }
            } else {
                pcb.sig_info_mut().set_tty(None);
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
            current_pcb.mark_exiting();
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
                let parent_ns = parent.active_pid_ns();
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
