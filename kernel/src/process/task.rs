use core::sync::atomic::{fence, AtomicBool, AtomicI32, AtomicU8, AtomicUsize, Ordering};

use alloc::{
    ffi::CString,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use log::{error, warn};
use system_error::SystemError;

use crate::{
    arch::{
        ipc::signal::{AtomicSignal, SigSet, Signal},
        process::ArchPCBInfo,
        CurrentIrqArch, SigStackArch,
    },
    cgroup::{cgroup_root_node, CgroupNode, TaskCgroupRef},
    driver::tty::tty_core::TtyCore,
    exception::InterruptArch,
    filesystem::{
        fs::FsStruct,
        vfs::{file::FileDescriptorVec, FileType, IndexNode},
    },
    ipc::{sighand::SigHand, signal::RestartBlock},
    libs::{
        futex::futex::RobustListHead,
        lock_free_flags::LockFreeFlags,
        mutex::{Mutex, MutexGuard},
        rwlock::{RwLock, RwLockReadGuard, RwLockUpgradableGuard, RwLockWriteGuard},
        rwsem::RwSem,
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::WaitQueue,
    },
    process::{
        cred::{Cred, INIT_CRED},
        kthread::WorkerPrivate,
        namespace::nsproxy::NsProxy,
        pid::{Pid, PidLink, PidType},
        resource::{RLimit64, RLimitID, RUsage},
        timer::AlarmTimer,
        AtomicRawPid, ExitState, KernelStack, ProcessBasicInfo, ProcessCpuTime, ProcessFlags,
        ProcessItimers, ProcessManager, ProcessSchedulerInfo, ProcessSignalInfo, ProcessState,
        RawPid, ThreadInfo, PTRACE_RELATION_LOCK,
    },
    rcu::RcuArcSlot,
};

use crate::process::{posix_timer, ptrace, rseq, seccomp};

#[derive(Debug)]
pub struct ProcessControlBlock {
    /// The PID of the current process.
    pub(super) pid: AtomicRawPid,
    /// The thread group ID of the current process (this value never changes
    /// within the same thread group).
    pub(super) tgid: RawPid,

    pub(super) thread_pid: RwLock<Option<Arc<Pid>>>,
    /// Array of PID links.
    pub(super) pid_links: [PidLink; PidType::PIDTYPE_MAX],

    /// Namespace proxy.
    pub(super) nsproxy: RcuArcSlot<NsProxy>,
    /// The cgroup (v2) this task belongs to.
    pub(super) task_cgroup: RwLock<TaskCgroupRef>,

    pub(super) basic: RwLock<ProcessBasicInfo>,
    /// Spinlock hold count of the current process.
    pub(super) preempt_count: AtomicUsize,
    /// Nesting count for no-fault user access sections.
    pub(super) pagefault_disabled: AtomicUsize,
    /// RCU read-side nesting depth of the current process.
    pub(super) rcu_read_depth: AtomicUsize,

    pub(super) flags: LockFreeFlags<ProcessFlags>,
    /// Whether the current task has been counted in the global visible thread
    /// count.
    pub(super) visible_thread_accounted: AtomicBool,
    /// Serializes task-local pointer publication for RCU-protected metadata
    /// such as `cred`, `nsproxy`, and `sighand`.
    pub(super) task_lock: SpinLock<()>,
    pub(super) worker_private: SpinLock<Option<WorkerPrivate>>,
    /// Kernel stack of the process.
    pub(super) kernel_stack: RwLock<KernelStack>,

    /// System call stack.
    pub(super) syscall_stack: RwLock<KernelStack>,

    /// Scheduling-related information.
    pub(super) sched_info: ProcessSchedulerInfo,
    /// Architecture-specific information.
    pub(super) arch_info: SpinLock<ArchPCBInfo>,
    /// Signal-handling related information (could potentially be lock-free).
    pub(super) sig_info: RwLock<ProcessSignalInfo>,
    pub(super) sighand: RcuArcSlot<SigHand>,
    /// Alternate signal stack.
    pub(super) sig_altstack: RwLock<SigStackArch>,
    /// Exit state (Running / Zombie / Dead).
    pub(super) exit_state: AtomicU8,

    /// Linux task_struct::exit_signal semantics:
    /// - -1: Non-thread-group leader (CLONE_THREAD);
    /// - 0: No exit signal sent, but still a waitable clone child;
    /// - >0: Signal number to send to the parent on exit.
    pub(super) exit_signal: AtomicI32,
    /// Signal to send to the current process when its parent exits
    /// (PR_SET_PDEATHSIG).
    pub(super) pdeath_signal: AtomicSignal,

    /// prctl(PR_SET/GET_NO_NEW_PRIVS) state: thread-level (task) semantics.
    pub(super) no_new_privs: AtomicBool,

    /// prctl(PR_SET/GET_KEEPCAPS) state: thread-level (task) semantics.
    /// When true, the process retains capabilities after changing UID/GID.
    pub(super) keepcaps: AtomicBool,

    /// prctl(PR_SET/GET_DUMPABLE) state.
    /// Linux: 0=SUID_DUMP_DISABLE, 1=SUID_DUMP_USER; 2 (SUID_DUMP_ROOT) is not
    /// allowed via PR_SET_DUMPABLE.
    pub(super) dumpable: AtomicU8,

    pub(super) seccomp_mode: AtomicU8,
    pub(super) seccomp_filter: SpinLock<Option<Arc<seccomp::SeccompFilter>>>,

    /// Parent process pointer.
    pub(super) parent_pcb: RwLock<Weak<ProcessControlBlock>>,
    /// Real (original) parent process pointer.
    pub(super) real_parent_pcb: RwLock<Weak<ProcessControlBlock>>,
    /// The natural parent pointer for wait operations.
    ///
    /// Linux's wait_task_zombie()/wait_consider_task() `__WNOTHREAD` depends on
    /// task_struct::parent, whereas DragonOS models parent_pcb/real_parent_pcb
    /// on the thread-group leader. This field preserves the thread-level parent
    /// relationship required by wait.
    pub(super) wait_parent_pcb: RwLock<Weak<ProcessControlBlock>>,
    /// Thread-level natural-parent compensation for PTRACE_TRACEME.
    ///
    /// On normal fork it points to the creating thread, compensating for the
    /// fact that real_parent_pcb is still modeled on the thread-group leader.
    /// After CLONE_PARENT, reparent, and de_thread it must be updated to follow
    /// Linux real_parent semantics. It is NOT an immutable “first-fork creator”
    /// record.
    pub(super) fork_parent_pcb: RwLock<Weak<ProcessControlBlock>>,

    /// Linked list of children processes.
    pub(super) children: RwLock<Vec<RawPid>>,
    /// Tasks currently traced by this process. Entries are global raw pids.
    pub(super) ptraced: RwLock<Vec<RawPid>>,
    /// Current tracer if this process is ptraced.
    pub(super) ptracer_pcb: RwLock<Weak<ProcessControlBlock>>,

    /// Wait queue.
    pub(super) wait_queue: WaitQueue,

    /// CPU-time wait queue: used for clock_nanosleep with
    /// CLOCK_{PROCESS,THREAD}_CPUTIME_ID.
    pub(super) cputime_wait_queue: WaitQueue,

    /// Thread information.
    pub(super) thread: RwLock<ThreadInfo>,

    /// Process filesystem state.
    pub(super) fs: RwLock<Option<Arc<FsStruct>>>,
    /// Serializes replacement/removal of `fs` with pivot_root's exact-path
    /// migration. Ordinary readers intentionally stay off this cold-path lock.
    fs_slot_update_lock: Mutex<()>,

    /// Alarm timer.
    pub(super) alarm_timer: SpinLock<Option<AlarmTimer>>,
    pub(super) itimers: SpinLock<ProcessItimers>,
    /// POSIX interval timers (timer_create / timer_settime / ...).
    pub(super) posix_timers: SpinLock<posix_timer::ProcessPosixTimers>,

    /// CPU time accounting.
    pub(super) cpu_time: Arc<ProcessCpuTime>,
    /// Thread-group-level resource accumulation for exited threads. Aligns with
    /// Linux signal_struct's exited thread statistics.
    pub(super) exited_thread_group_rusage: SpinLock<RUsage>,
    /// Resource accumulation for children successfully reaped by the wait family;
    /// aligns with getrusage(RUSAGE_CHILDREN).
    pub(super) children_rusage: SpinLock<RUsage>,

    /// Process robust lock list.
    pub(super) robust_list: RwLock<Option<RobustListHead>>,

    /// rseq (Restartable Sequences) state.
    pub(super) rseq_state: RwLock<rseq::RseqState>,

    /// Credential set for the process as a subject.
    pub(super) cred: RcuArcSlot<Cred>,
    pub(super) self_ref: Weak<ProcessControlBlock>,

    pub(super) restart_block: SpinLock<Option<RestartBlock>>,

    /// Path to the process's executable file.
    pub(super) executable_path: RwLock<String>,
    /// Process command line (used for /proc/<pid>/cmdline; Linux semantics: argv
    /// entries are separated by '\0').
    pub(super) cmdline: RwLock<Vec<u8>>,
    /// Resource limit (rlimit) array.
    pub(super) rlimits: RwLock<[RLimit64; RLimitID::Nlimits as usize]>,
}

impl ProcessControlBlock {
    /// Create a new PCB.
    ///
    /// ## Parameters
    ///
    /// - `name`: The process name.
    /// - `kstack`: The kernel stack for the process.
    ///
    /// ## Returns
    ///
    /// A new PCB.
    pub fn new(name: String, kstack: KernelStack) -> Arc<Self> {
        return Self::do_create_pcb(name, kstack, false);
    }

    /// Create a new idle process.
    ///
    /// Note: This function must only be called during process manager
    /// initialization.
    pub fn new_idle(cpu_id: u32, kstack: KernelStack) -> Arc<Self> {
        let name = format!("idle-{}", cpu_id);
        return Self::do_create_pcb(name, kstack, true);
    }

    /// Returns whether the process is a kernel thread.
    ///
    /// # Returns
    ///
    /// `true` if the process is a kernel thread, `false` otherwise.
    pub fn is_kthread(&self) -> bool {
        self.flags().contains(ProcessFlags::KTHREAD)
    }

    #[inline(never)]
    fn do_create_pcb(name: String, kstack: KernelStack, is_idle: bool) -> Arc<Self> {
        // Initialize the namespace proxy.
        let nsproxy = if is_idle {
            // The idle process uses the root namespace.
            NsProxy::new_root()
        } else {
            // Other processes inherit their parent's namespace.
            ProcessManager::current_pcb().nsproxy().clone()
        };
        let task_cgroup = if is_idle {
            TaskCgroupRef::new(cgroup_root_node())
        } else {
            ProcessManager::current_pcb().task_cgroup_ref()
        };

        let (raw_pid, ppid, cwd, cred, tty): (
            RawPid,
            RawPid,
            String,
            Arc<Cred>,
            Option<Arc<TtyCore>>,
        ) = if is_idle {
            let cred = INIT_CRED.clone();
            (RawPid(0), RawPid(0), "/".to_string(), cred, None)
        } else {
            let ppid = ProcessManager::current_pcb().task_pid_vnr();
            let cred = ProcessManager::current_pcb().cred();

            let cwd = ProcessManager::current_pcb().basic().cwd();
            let tty = ProcessManager::current_pcb().sig_info_irqsave().tty();

            // Here, UNASSIGNED is used to represent an unallocated pid,
            // which will be allocated later in `copy_process`.
            let raw_pid = RawPid::UNASSIGNED;

            (raw_pid, ppid, cwd, cred, tty)
        };

        let basic_info = ProcessBasicInfo::new(ppid, name.clone(), cwd, None);
        let preempt_count = AtomicUsize::new(0);
        let pagefault_disabled = AtomicUsize::new(0);
        let rcu_read_depth = AtomicUsize::new(0);
        let flags = unsafe { LockFreeFlags::new(ProcessFlags::empty()) };
        let initial_sighand = SigHand::new();
        initial_sighand.attach_task_ref();

        let sched_info = ProcessSchedulerInfo::new(None);

        let ppcb: Weak<ProcessControlBlock> = ProcessManager::find_task_by_vpid(ppid)
            .map(|p| Arc::downgrade(&p))
            .unwrap_or_default();

        // Use Arc::new_cyclic to avoid constructing a large struct on the stack.
        let pcb = Arc::new_cyclic(|weak| {
            let arch_info = SpinLock::new(ArchPCBInfo::new(&kstack));

            let pcb = Self {
                pid: AtomicRawPid::new(raw_pid),
                tgid: raw_pid,
                thread_pid: RwLock::new(None),
                pid_links: core::array::from_fn(|_| PidLink::default()),
                nsproxy: RcuArcSlot::new(nsproxy),
                task_cgroup: RwLock::new(task_cgroup),
                basic: basic_info,
                preempt_count,
                pagefault_disabled,
                rcu_read_depth,
                flags,
                visible_thread_accounted: AtomicBool::new(false),
                task_lock: SpinLock::new(()),
                kernel_stack: RwLock::new(kstack),
                syscall_stack: RwLock::new(KernelStack::new().unwrap()),
                worker_private: SpinLock::new(None),
                sched_info,
                arch_info,
                sig_info: RwLock::new(ProcessSignalInfo::default()),
                sighand: RcuArcSlot::new(initial_sighand.clone()),
                sig_altstack: RwLock::new(SigStackArch::new()),
                exit_state: AtomicU8::new(ExitState::Running as u8),
                exit_signal: AtomicI32::new(Signal::SIGCHLD as i32),
                pdeath_signal: AtomicSignal::new(Signal::INVALID),

                no_new_privs: AtomicBool::new(false),
                keepcaps: AtomicBool::new(false),
                // Default to SUID_DUMP_USER(=1) to satisfy gVisor's
                // SetGetDumpability expectation.
                dumpable: AtomicU8::new(1),
                seccomp_mode: AtomicU8::new(seccomp::SeccompMode::Disabled as u8),
                seccomp_filter: SpinLock::new(None),
                parent_pcb: RwLock::new(ppcb.clone()),
                real_parent_pcb: RwLock::new(ppcb.clone()),
                wait_parent_pcb: RwLock::new(ppcb.clone()),
                fork_parent_pcb: RwLock::new(ppcb),
                children: RwLock::new(Vec::new()),
                ptraced: RwLock::new(Vec::new()),
                ptracer_pcb: RwLock::new(Weak::new()),
                wait_queue: WaitQueue::default(),
                cputime_wait_queue: WaitQueue::default(),
                thread: RwLock::new(ThreadInfo::new()),
                fs: RwLock::new(Some(Arc::new(FsStruct::new()))),
                fs_slot_update_lock: Mutex::new(()),
                alarm_timer: SpinLock::new(None),
                itimers: SpinLock::new(ProcessItimers::default()),
                posix_timers: SpinLock::new(posix_timer::ProcessPosixTimers::default()),
                cpu_time: Arc::new(ProcessCpuTime::default()),
                exited_thread_group_rusage: SpinLock::new(RUsage::default()),
                children_rusage: SpinLock::new(RUsage::default()),
                robust_list: RwLock::new(None),
                rseq_state: RwLock::new(rseq::RseqState::new()),
                cred: RcuArcSlot::new(cred),
                self_ref: weak.clone(),
                restart_block: SpinLock::new(None),
                executable_path: RwLock::new(name),
                cmdline: RwLock::new(Vec::new()),
                rlimits: RwLock::new(Self::default_rlimits()),
            };

            pcb.sig_info.write().set_tty(tty);

            // Initialize the system call stack.
            #[cfg(target_arch = "x86_64")]
            pcb.arch_info
                .lock()
                .init_syscall_stack(&pcb.syscall_stack.read());

            pcb
        });

        pcb.sched_info()
            .sched_entity()
            .force_mut()
            .set_pcb(Arc::downgrade(&pcb));
        // Store the process's Arc pointer at the lowest address of the kernel
        // stack and system call stack.
        unsafe {
            pcb.kernel_stack
                .write()
                .set_pcb(Arc::downgrade(&pcb))
                .unwrap();

            pcb.syscall_stack
                .write()
                .set_pcb(Arc::downgrade(&pcb))
                .unwrap()
        };

        return pcb;
    }

    fn default_rlimits() -> [crate::process::resource::RLimit64; RLimitID::Nlimits as usize] {
        use crate::mm::ucontext::UserStack;
        use crate::process::resource::{RLimit64, RLimitID};

        let mut arr = [RLimit64 {
            rlim_cur: 0,
            rlim_max: 0,
        }; RLimitID::Nlimits as usize];

        // Linux typical defaults: soft limit 1024, hard limit adjustable via
        // setrlimit. The file descriptor table auto-expands based on
        // RLIMIT_NOFILE.
        arr[RLimitID::Nofile as usize] = RLimit64 {
            rlim_cur: FileDescriptorVec::MAX_CAPACITY as u64,
            rlim_max: FileDescriptorVec::MAX_CAPACITY as u64,
        };

        arr[RLimitID::Stack as usize] = RLimit64 {
            rlim_cur: UserStack::DEFAULT_USER_STACK_SIZE as u64,
            rlim_max: UserStack::DEFAULT_USER_STACK_SIZE as u64,
        };

        arr[RLimitID::As as usize] = {
            let end = <crate::arch::MMArch as crate::mm::MemoryManagementArch>::USER_END_VADDR;
            RLimit64 {
                rlim_cur: end.data() as u64,
                rlim_max: end.data() as u64,
            }
        };
        arr[RLimitID::Rss as usize] = arr[RLimitID::As as usize];

        // Set the default file size limit (Linux typically defaults to unlimited).
        arr[RLimitID::Fsize as usize] = RLimit64 {
            rlim_cur: u64::MAX,
            rlim_max: u64::MAX,
        };

        // Linux commonly defaults RLIMIT_MEMLOCK to 64 KiB. Keeping the hard
        // limit non-zero also allows unprivileged tests to lower the soft limit.
        arr[RLimitID::Memlock as usize] = RLimit64 {
            rlim_cur: 64 * 1024,
            rlim_max: 64 * 1024,
        };

        arr
    }

    #[inline(always)]
    pub fn get_rlimit(&self, res: RLimitID) -> crate::process::resource::RLimit64 {
        self.rlimits.read()[res as usize]
    }

    pub fn set_rlimit(
        &self,
        res: RLimitID,
        newv: crate::process::resource::RLimit64,
    ) -> Result<(), system_error::SystemError> {
        use system_error::SystemError;
        if newv.rlim_cur > newv.rlim_max {
            return Err(SystemError::EINVAL);
        }

        // Note: RLIMIT_NOFILE is allowed to be 0, as expected by test cases.
        // When rlim_cur is 0, no new file descriptors can be allocated, but
        // existing fds remain usable.

        // For RLIMIT_NOFILE, check against the system's maximum capacity limit.
        if res == RLimitID::Nofile {
            if newv.rlim_cur > FileDescriptorVec::MAX_CAPACITY as u64 {
                return Err(SystemError::EINVAL);
            }
            if newv.rlim_max > FileDescriptorVec::MAX_CAPACITY as u64 {
                return Err(SystemError::EINVAL);
            }
        }

        let cur = self.rlimits.read()[res as usize];
        if newv.rlim_max > cur.rlim_max {
            let cred = self.cred();
            if !cred.has_capability(crate::process::cred::CAPFlags::CAP_SYS_RESOURCE) {
                return Err(SystemError::EPERM);
            }
        }

        // Update the rlimit.
        self.rlimits.write()[res as usize] = newv;

        // If RLIMIT_NOFILE changed, adjust the file descriptor table.
        if res == RLimitID::Nofile {
            if let Err(e) = self.adjust_fd_table_for_rlimit_change(newv.rlim_cur as usize) {
                // If adjustment fails, roll back the rlimit.
                self.rlimits.write()[res as usize] = cur;
                return Err(e);
            }
        }

        Ok(())
    }

    /// Inherit all rlimits from the parent process.
    pub fn inherit_rlimits_from(&self, parent: &Arc<ProcessControlBlock>) {
        let src = *parent.rlimits.read();
        *self.rlimits.write() = src;

        // After inheritance, adjust the file descriptor table to match the new
        // RLIMIT_NOFILE.
        let nofile_limit = src[RLimitID::Nofile as usize].rlim_cur as usize;
        if let Err(e) = self.adjust_fd_table_for_rlimit_change(nofile_limit) {
            // If adjustment fails, log the error but do not block inheritance.
            error!(
                "Failed to adjust fd table after inheriting rlimits: {:?}",
                e
            );
        }
    }

    /// Adjust the file descriptor table when RLIMIT_NOFILE changes.
    ///
    /// ## Parameters
    /// - `new_rlimit_nofile`: The new RLIMIT_NOFILE value.
    ///
    /// ## Returns
    /// - `Ok(())`: Adjustment succeeded.
    /// - `Err(SystemError)`: Adjustment failed.
    fn adjust_fd_table_for_rlimit_change(
        &self,
        new_rlimit_nofile: usize,
    ) -> Result<(), system_error::SystemError> {
        let fd_table = self.basic.read().try_fd_table().unwrap();
        let mut fd_table_guard = fd_table.write();
        fd_table_guard.adjust_for_rlimit_change(new_rlimit_nofile)
    }

    /// Returns the current process's lock hold count.
    #[inline(always)]
    pub fn preempt_count(&self) -> usize {
        return self.preempt_count.load(Ordering::SeqCst);
    }

    /// Returns the current task's no-fault user access nesting depth.
    #[inline(always)]
    pub fn pagefault_disabled(&self) -> usize {
        self.pagefault_disabled.load(Ordering::SeqCst)
    }

    #[inline(always)]
    pub fn rcu_read_depth(&self) -> usize {
        self.rcu_read_depth.load(Ordering::SeqCst)
    }

    /// Increments the current process's lock hold count.
    #[inline(always)]
    pub fn preempt_disable(&self) {
        self.preempt_count.fetch_add(1, Ordering::SeqCst);
    }

    /// Decrements the current process's lock hold count.
    #[inline(always)]
    pub fn preempt_enable(&self) {
        self.preempt_count.fetch_sub(1, Ordering::SeqCst);
    }

    #[inline(always)]
    pub fn pagefault_disable(&self) {
        self.pagefault_disabled.fetch_add(1, Ordering::SeqCst);
    }

    #[inline(always)]
    pub fn pagefault_enable(&self) {
        self.pagefault_disabled.fetch_sub(1, Ordering::SeqCst);
    }

    #[inline(always)]
    pub fn rcu_read_lock(&self) {
        self.rcu_read_depth.fetch_add(1, Ordering::SeqCst);
    }

    #[inline(always)]
    pub fn rcu_read_unlock(&self) {
        let mut current = self.rcu_read_depth.load(Ordering::SeqCst);
        loop {
            assert!(current > 0, "rcu_read_unlock underflow");
            match self.rcu_read_depth.compare_exchange(
                current,
                current - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    #[inline(always)]
    pub unsafe fn set_preempt_count(&self, count: usize) {
        self.preempt_count.store(count, Ordering::SeqCst);
    }

    #[inline(always)]
    pub fn contain_child(&self, pid: &RawPid) -> bool {
        let children = self.children.read();
        return children.contains(pid);
    }

    #[inline(always)]
    pub fn flags(&self) -> &mut ProcessFlags {
        return self.flags.get_mut();
    }

    #[inline(always)]
    pub(crate) fn mark_visible_thread_accounted(&self) {
        self.visible_thread_accounted.store(true, Ordering::Release);
    }

    #[inline(always)]
    pub(crate) fn take_visible_thread_accounted(&self) -> bool {
        self.visible_thread_accounted.swap(false, Ordering::AcqRel)
    }

    /// Note: this value can be read from interrupt context, but must not be
    /// modified from interrupt context, otherwise a deadlock will occur.
    #[inline(always)]
    pub fn basic(&self) -> RwLockReadGuard<'_, ProcessBasicInfo> {
        return self.basic.read_irqsave();
    }

    #[inline(always)]
    pub fn set_name(&self, name: String) {
        self.basic.write().set_name(name);
    }

    #[inline(always)]
    pub fn set_pdeath_signal(&self, signal: Signal) {
        self.pdeath_signal.store(signal, Ordering::SeqCst);
    }

    #[inline(always)]
    pub fn pdeath_signal(&self) -> Signal {
        self.pdeath_signal.load(Ordering::SeqCst)
    }

    #[inline(always)]
    pub fn no_new_privs(&self) -> usize {
        if self.no_new_privs.load(Ordering::SeqCst) {
            1
        } else {
            0
        }
    }

    #[inline(always)]
    pub fn set_no_new_privs(&self, value: bool) {
        // Linux semantics: once no_new_privs is set, it cannot be cleared.
        if value {
            self.no_new_privs.store(true, Ordering::SeqCst);
        }
    }

    #[inline(always)]
    pub fn keepcaps(&self) -> bool {
        self.keepcaps.load(Ordering::SeqCst)
    }

    #[inline(always)]
    pub fn set_keepcaps(&self, value: bool) {
        self.keepcaps.store(value, Ordering::SeqCst);
    }

    #[inline(always)]
    pub fn dumpable(&self) -> u8 {
        self.dumpable.load(Ordering::SeqCst)
    }

    #[inline(always)]
    pub fn set_dumpable(&self, value: u8) {
        self.dumpable.store(value, Ordering::SeqCst)
    }

    #[inline(always)]
    pub fn seccomp_mode(&self) -> seccomp::SeccompMode {
        seccomp::SeccompMode::from(self.seccomp_mode.load(Ordering::Relaxed))
    }

    #[inline(always)]
    pub fn set_seccomp_mode(&self, mode: seccomp::SeccompMode) {
        self.seccomp_mode.store(mode as u8, Ordering::SeqCst);
    }

    #[inline(always)]
    pub fn seccomp_filter_lock(&self) -> SpinLockGuard<'_, Option<Arc<seccomp::SeccompFilter>>> {
        self.seccomp_filter.lock()
    }

    #[inline(always)]
    pub fn basic_mut(&self) -> RwLockWriteGuard<'_, ProcessBasicInfo> {
        return self.basic.write_irqsave();
    }

    /// Acquires the arch_info lock with interrupts disabled.
    #[inline(always)]
    pub fn arch_info_irqsave(&self) -> SpinLockGuard<'_, ArchPCBInfo> {
        return self.arch_info.lock_irqsave();
    }

    /// Acquires the arch_info lock without disabling interrupts.
    ///
    /// Because arch_info is used during context switching, acquiring it outside
    /// of interrupt context without irqsave is unsafe.
    ///
    /// This function may only be used in the following cases:
    /// - In interrupt context (interrupts already disabled).
    /// - Immediately after creating a new PCB.
    #[inline(always)]
    pub unsafe fn arch_info(&self) -> SpinLockGuard<'_, ArchPCBInfo> {
        return self.arch_info.lock();
    }

    #[inline(always)]
    pub fn kernel_stack(&self) -> RwLockReadGuard<'_, KernelStack> {
        return self.kernel_stack.read();
    }

    pub unsafe fn kernel_stack_force_ref(&self) -> &KernelStack {
        self.kernel_stack.force_get_ref()
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub fn kernel_stack_mut(&self) -> RwLockWriteGuard<'_, KernelStack> {
        return self.kernel_stack.write();
    }

    #[inline(always)]
    pub fn sched_info(&self) -> &ProcessSchedulerInfo {
        return &self.sched_info;
    }

    pub fn sig_altstack(&self) -> RwLockReadGuard<'_, SigStackArch> {
        self.sig_altstack.read_irqsave()
    }

    pub fn sig_altstack_mut(&self) -> RwLockWriteGuard<'_, SigStackArch> {
        self.sig_altstack.write_irqsave()
    }

    #[inline(always)]
    pub fn worker_private(&self) -> SpinLockGuard<'_, Option<WorkerPrivate>> {
        return self.worker_private.lock();
    }

    #[inline(always)]
    pub fn raw_pid(&self) -> RawPid {
        return self.pid.load(Ordering::Acquire);
    }

    #[inline(always)]
    pub fn raw_tgid(&self) -> RawPid {
        return self.tgid;
    }

    #[inline(always)]
    pub fn fs_struct(&self) -> Arc<FsStruct> {
        self.try_fs_struct()
            .expect("live task must have an fs_struct")
    }

    /// Return the filesystem context when the task has not passed exit_fs().
    pub fn try_fs_struct(&self) -> Option<Arc<FsStruct>> {
        self.fs.read().clone()
    }

    #[inline(always)]
    pub fn fs_struct_is_shared(&self) -> bool {
        Arc::strong_count(
            self.fs
                .read()
                .as_ref()
                .expect("live task must have an fs_struct"),
        ) > 1
    }

    pub(crate) fn set_fs_struct(
        &self,
        fs: Arc<FsStruct>,
        _fs_refs: &super::FsRefsReadGuard,
    ) -> Arc<FsStruct> {
        let _slot_update = self.fs_slot_update_lock.lock();
        let mut guard = self.fs.write();
        let old = guard.replace(fs).expect("live task must have an fs_struct");
        old
    }

    /// Drop this task's reference to its filesystem context during exit.
    ///
    /// This mirrors Linux `exit_fs()`: a zombie retains process metadata for
    /// wait(2), but must no longer pin its root or working-directory mounts.
    pub(super) fn exit_fs(&self) {
        let _slot_update = self.fs_slot_update_lock.lock();
        let fs = self.fs.write().take();
        // Release the spin-based slot guard before dropping the final FsStruct
        // owner, whose path-pin destructors may enqueue deferred cleanup work.
        drop(fs);
    }

    /// Stabilize this task's fs slot across an operation that may sleep while
    /// updating the referenced FsStruct.
    pub(crate) fn lock_fs_slot_update(&self) -> MutexGuard<'_, ()> {
        self.fs_slot_update_lock.lock()
    }

    pub fn pwd_inode(&self) -> Arc<dyn IndexNode> {
        self.fs_struct().pwd()
    }

    /// Returns an `Arc` pointer to the file descriptor table.
    #[inline(always)]
    pub fn fd_table(&self) -> Arc<RwSem<FileDescriptorVec>> {
        return self.basic.read().try_fd_table().unwrap();
    }

    #[inline(always)]
    pub fn cred(&self) -> Arc<Cred> {
        self.cred.load()
    }

    /// Atomically replace the current process's credential set (cred).
    ///
    /// - Uses irqsave write lock for concurrency safety.
    /// - Returns `Result` so that callers can extend error handling as needed.
    pub fn set_cred(&self, new: Arc<Cred>) -> Result<(), SystemError> {
        let _task_guard = self.task_lock.lock_irqsave();
        self.cred.store_deferred(new);
        Ok(())
    }

    pub fn set_execute_path(&self, path: String) {
        *self.executable_path.write() = path;
    }

    pub fn execute_path(&self) -> String {
        self.executable_path.read().clone()
    }

    /// Returns the raw byte sequence for /proc/<pid>/cmdline (argv entries
    /// separated by '\0').
    #[inline(always)]
    pub fn cmdline_bytes(&self) -> Vec<u8> {
        self.cmdline.read().clone()
    }

    /// Directly set cmdline (used for fork inheritance and similar scenarios).
    #[inline(always)]
    pub fn set_cmdline_bytes(&self, data: Vec<u8>) {
        *self.cmdline.write() = data;
    }

    /// Write argv after a successful exec (Linux semantics: each argument is
    /// NUL-terminated; the entire buffer is typically NUL-terminated as well).
    #[inline(never)]
    pub fn set_cmdline_from_argv(&self, argv: &[CString]) {
        let mut buf: Vec<u8> = Vec::new();
        for arg in argv {
            buf.extend_from_slice(arg.as_bytes());
            buf.push(0);
        }
        *self.cmdline.write() = buf;
    }

    pub fn real_parent_pcb(&self) -> Option<Arc<ProcessControlBlock>> {
        return self.real_parent_pcb.read_irqsave().upgrade();
    }

    pub fn ptracer_pcb(&self) -> Option<Arc<ProcessControlBlock>> {
        ptrace::ptracer_of(&self.self_ref.upgrade()?)
    }

    pub fn is_ptraced(&self) -> bool {
        ptrace::is_ptraced(self)
    }

    pub fn ptraced_pids(&self) -> Vec<RawPid> {
        let Some(this) = self.self_ref.upgrade() else {
            return Vec::new();
        };
        ptrace::tracees_of(&this)
    }

    pub fn fork_parent_pcb(&self) -> Option<Arc<ProcessControlBlock>> {
        self.fork_parent_pcb.read_irqsave().upgrade()
    }

    pub fn wait_parent_pcb(&self) -> Option<Arc<ProcessControlBlock>> {
        self.wait_parent_pcb.read_irqsave().upgrade()
    }

    /// Returns whether the current process is the global init process.
    pub fn is_global_init(&self) -> bool {
        self.task_tgid_vnr().unwrap() == RawPid(1)
    }

    /// Get the `Arc` pointer to the socket's `IndexNode` by file descriptor.
    ///
    /// This is a helper function.
    ///
    /// ## Parameters
    ///
    /// - `fd`: The file descriptor index.
    ///
    /// ## Returns
    ///
    /// The `Arc` pointer to the socket's `IndexNode` if the file descriptor
    /// refers to a socket, otherwise an error code.
    ///
    /// # Note
    /// Because the underlying `Socket` may contain generics, generic type
    /// information is lost after type erasure to `Arc<dyn Socket>`. Therefore
    /// this function returns `Arc<dyn IndexNode>`, which can be converted to
    /// `Option<&dyn Socket>` via `as_socket()` at the call site. Since the
    /// conversion has already been checked internally, the caller can directly
    /// `unwrap` to obtain `&dyn Socket`.
    pub fn get_socket_inode(&self, fd: i32) -> Result<Arc<dyn IndexNode>, SystemError> {
        let f = ProcessManager::current_pcb()
            .fd_table()
            .read()
            .get_file_by_fd(fd)
            .ok_or({
                // log::warn!("get_socket: fd {} not found", fd);
                SystemError::EBADF
            })?;

        if f.file_type() != FileType::Socket {
            return Err(SystemError::ENOTSOCK);
        }

        let inode = f.inode();
        // log::info!("get_socket: fd {} is a socket", fd);
        if let Some(_sock) = inode.as_socket() {
            // log::info!("{:?}", sock);
            return Ok(inode);
        }

        Err(SystemError::ENOTSOCK)
    }

    fn is_alive_reparent_target(pcb: &Arc<ProcessControlBlock>) -> bool {
        !pcb.flags().contains(ProcessFlags::EXITING)
            && !pcb.is_exited()
            && !pcb.is_zombie()
            && !pcb.is_dead()
    }

    fn find_alive_thread_in_group(
        pcb: Arc<ProcessControlBlock>,
    ) -> Option<Arc<ProcessControlBlock>> {
        ProcessManager::thread_group_tasks_snapshot(pcb)
            .into_iter()
            .find(ProcessControlBlock::is_alive_reparent_target)
    }

    fn find_alive_thread_reaper(
        exiting: &Arc<ProcessControlBlock>,
    ) -> Option<Arc<ProcessControlBlock>> {
        ProcessManager::thread_group_tasks_snapshot(exiting.clone())
            .into_iter()
            .find(|task| {
                !Arc::ptr_eq(task, exiting) && ProcessControlBlock::is_alive_reparent_target(task)
            })
    }

    fn child_wait_parent_is(
        child: &Arc<ProcessControlBlock>,
        parent: &Arc<ProcessControlBlock>,
    ) -> bool {
        child
            .wait_parent_pcb()
            .as_ref()
            .map(|wait_parent| Arc::ptr_eq(wait_parent, parent))
            .unwrap_or(false)
    }

    fn link_child_to_parent_list(
        child: &Arc<ProcessControlBlock>,
        parent: &Arc<ProcessControlBlock>,
    ) {
        let child_vpid = child
            .task_pid_nr_ns(PidType::PID, Some(parent.active_pid_ns()))
            .unwrap_or(RawPid::new(0));
        if child_vpid.data() == 0 {
            return;
        }

        let mut children = parent.children.write_irqsave();
        if !children.contains(&child_vpid) {
            children.push(child_vpid);
        }
    }

    pub(crate) fn reparent_child_to(
        child: &Arc<ProcessControlBlock>,
        new_parent: &Arc<ProcessControlBlock>,
    ) {
        let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
        ProcessControlBlock::reparent_child_to_locked(child, new_parent);
    }

    fn reparent_child_to_locked(
        child: &Arc<ProcessControlBlock>,
        new_parent: &Arc<ProcessControlBlock>,
    ) {
        *child.parent_pcb.write_irqsave() = Arc::downgrade(new_parent);
        *child.real_parent_pcb.write_irqsave() = Arc::downgrade(new_parent);
        *child.wait_parent_pcb.write_irqsave() = Arc::downgrade(new_parent);
        *child.fork_parent_pcb.write_irqsave() = Arc::downgrade(new_parent);

        let parent_pid_in_child_ns = new_parent
            .task_pid_nr_ns(PidType::PID, Some(child.active_pid_ns()))
            .unwrap_or(RawPid::new(0));
        child.basic.write_irqsave().ppid = parent_pid_in_child_ns;

        ProcessControlBlock::link_child_to_parent_list(child, new_parent);

        // Linux reparent_leader() notifies the new parent when a zombie child
        // becomes wait-visible through reparenting. DragonOS keeps per-task wait
        // queues, so wake both the concrete new parent and its group leader.
        if child.is_zombie() {
            ProcessManager::wake_wait_parent(new_parent);
        }
    }

    pub(crate) fn reparent_child_links_from_thread_group(
        child: &Arc<ProcessControlBlock>,
        old_tgid: RawPid,
        new_parent: &Arc<ProcessControlBlock>,
    ) {
        let should_reparent = child
            .parent_pcb()
            .as_ref()
            .map(|parent| parent.tgid == old_tgid)
            .unwrap_or(false)
            || child
                .real_parent_pcb()
                .as_ref()
                .map(|parent| parent.tgid == old_tgid)
                .unwrap_or(false)
            || child
                .wait_parent_pcb()
                .as_ref()
                .map(|parent| parent.tgid == old_tgid)
                .unwrap_or(false);

        if should_reparent {
            ProcessControlBlock::reparent_child_to(child, new_parent);
        }
    }

    fn collect_children_for_exit(
        exiting: &Arc<ProcessControlBlock>,
    ) -> Vec<Arc<ProcessControlBlock>> {
        // Caller holds PTRACE_RELATION_LOCK so collecting children, selecting a
        // reaper, and moving children form one tasklist-lock-like transaction.
        let mut result = Vec::new();
        let mut seen = Vec::new();

        let mut push_reparent_child =
            |child: Arc<ProcessControlBlock>, result: &mut Vec<Arc<ProcessControlBlock>>| {
                if seen.iter().any(|pid| *pid == child.raw_pid()) {
                    return;
                }
                seen.push(child.raw_pid());
                result.push(child);
            };

        for owner in ProcessManager::thread_group_tasks_snapshot(exiting.clone()) {
            let owner_ns = owner.active_pid_ns();
            let pids_to_rehome: Vec<RawPid> = if Arc::ptr_eq(&owner, exiting) {
                let mut children = owner.children.write_irqsave();
                core::mem::take(&mut *children)
            } else {
                let mut children = owner.children.write_irqsave();
                let mut removed = Vec::new();
                children.retain(|pid| {
                    let should_remove = ProcessManager::find_task_by_pid_ns(*pid, &owner_ns)
                        .as_ref()
                        .map(|child| ProcessControlBlock::child_wait_parent_is(child, exiting))
                        .unwrap_or(false);
                    if should_remove {
                        removed.push(*pid);
                    }
                    !should_remove
                });
                removed
            };

            for pid in pids_to_rehome {
                let Some(child) = ProcessManager::find_task_by_pid_ns(pid, &owner_ns) else {
                    continue;
                };

                if let Some(wait_parent) = child.wait_parent_pcb() {
                    if !Arc::ptr_eq(&wait_parent, exiting)
                        && wait_parent.tgid == exiting.tgid
                        && ProcessControlBlock::is_alive_reparent_target(&wait_parent)
                    {
                        ProcessControlBlock::reparent_child_to_locked(&child, &wait_parent);
                        continue;
                    }
                }

                push_reparent_child(child, &mut result);
            }
        }

        result
    }

    /// When the current process exits, have another thread, a subreaper, or init
    /// adopt its children.
    pub(super) unsafe fn adopt_childen(&self) -> Result<(), SystemError> {
        let exiting = self.self_ref.upgrade().ok_or(SystemError::ESRCH)?;
        let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
        let children = ProcessControlBlock::collect_children_for_exit(&exiting);

        if children.is_empty() {
            return Ok(());
        }

        self.notify_parent_exit_for_children(&children);

        if let Some(reaper) = ProcessControlBlock::find_alive_thread_reaper(&exiting) {
            for child in children {
                ProcessControlBlock::reparent_child_to_locked(&child, &reaper);
            }
            return Ok(());
        }

        let exiting_ns = exiting.active_pid_ns();
        let init_pcb = ProcessManager::find_task_by_pid_ns(RawPid(1), &exiting_ns)
            .ok_or(SystemError::ECHILD)?;

        // If the current process is the namespace init, children are adopted by
        // the init of the parent's pidns.
        if Arc::ptr_eq(&exiting, &init_pcb) {
            if let Some(parent_pcb) = self.real_parent_pcb() {
                assert!(
                    !Arc::ptr_eq(&parent_pcb, &init_pcb),
                    "adopt_childen: parent_pcb is init_pcb, pid: {}",
                    self.raw_pid()
                );
                let parent_init =
                    ProcessManager::find_task_by_pid_ns(RawPid(1), &parent_pcb.active_pid_ns());
                if let Some(parent_init) = parent_init {
                    for child in children {
                        ProcessControlBlock::reparent_child_to_locked(&child, &parent_init);
                    }
                }
            }
            return Ok(());
        }

        // Normal case: preferentially reparent to the “nearest ancestor
        // subreaper,” otherwise reparent to init.
        let mut reaper: Arc<ProcessControlBlock> = init_pcb.clone();
        let mut cursor = self.real_parent_pcb();
        while let Some(p) = cursor {
            // Linux semantics: child_subreaper is thread-group-level; the
            // thread-group leader is used uniformly for the check here.
            let leader = {
                let ti = p.threads_read_irqsave();
                ti.group_leader().unwrap_or_else(|| p.clone())
            };

            let visible_in_exiting_ns = leader
                .task_pid_nr_ns(PidType::PID, Some(exiting_ns.clone()))
                .is_some_and(|pid| pid.data() != 0);
            // Linux uses task_pid(reaper)->level == ns_level. DragonOS exposes
            // this through active pidns level plus visibility in exiting_ns.
            if !visible_in_exiting_ns || leader.active_pid_ns().level() != exiting_ns.level() {
                break;
            }

            if leader.sig_info_irqsave().is_child_subreaper() {
                if let Some(alive) = ProcessControlBlock::find_alive_thread_in_group(leader.clone())
                {
                    reaper = alive;
                    break;
                }
            }

            if Arc::ptr_eq(&leader, &init_pcb) {
                break;
            }

            cursor = leader.real_parent_pcb();
        }

        for child in children {
            ProcessControlBlock::reparent_child_to_locked(&child, &reaper);
        }

        Ok(())
    }

    fn notify_parent_exit_for_children(&self, children: &[Arc<ProcessControlBlock>]) {
        for child in children {
            let sig = child.pdeath_signal();
            if sig == Signal::INVALID {
                continue;
            }
            if let Err(e) = crate::ipc::kill::send_signal_to_pcb(child.clone(), sig) {
                warn!(
                    "adopt_childen: failed to deliver pdeath_signal {:?} to child {:?}: {:?}",
                    sig,
                    child.raw_pid(),
                    e
                );
            }
        }
    }

    /// Generate the process name from a program path.
    pub fn generate_name(program_path: &str) -> String {
        // Extract just the basename from the program path
        let name = program_path.split('/').next_back().unwrap_or(program_path);
        name.to_string()
    }

    pub fn sig_info_irqsave(&self) -> RwLockReadGuard<'_, ProcessSignalInfo> {
        self.sig_info.read_irqsave()
    }

    pub fn sig_info_upgradable(&self) -> RwLockUpgradableGuard<'_, ProcessSignalInfo> {
        self.sig_info.upgradeable_read_irqsave()
    }

    pub fn try_siginfo_irqsave(&self, times: u8) -> Option<RwLockReadGuard<'_, ProcessSignalInfo>> {
        for _ in 0..times {
            if let Some(r) = self.sig_info.try_read_irqsave() {
                return Some(r);
            }
        }

        return None;
    }

    pub fn sig_info_mut(&self) -> RwLockWriteGuard<'_, ProcessSignalInfo> {
        self.sig_info.write_irqsave()
    }

    pub fn is_active_vfork(&self) -> bool {
        self.thread.read_irqsave().vfork_done.is_some()
    }

    /// Returns a read-only reference to the rseq state.
    #[inline]
    pub fn rseq_state(&self) -> RwLockReadGuard<'_, rseq::RseqState> {
        self.rseq_state.read_irqsave()
    }

    /// Returns a mutable reference to the rseq state.
    #[inline]
    pub fn rseq_state_mut(&self) -> RwLockWriteGuard<'_, rseq::RseqState> {
        self.rseq_state.write_irqsave()
    }

    pub fn try_siginfo_mut(&self, times: u8) -> Option<RwLockWriteGuard<'_, ProcessSignalInfo>> {
        for _ in 0..times {
            if let Some(r) = self.sig_info.try_write_irqsave() {
                return Some(r);
            }
        }

        return None;
    }

    /// Returns whether the current process has any pending signals.
    pub fn has_pending_signal(&self) -> bool {
        let sig_info = self.sig_info_irqsave();
        let has_pending_thread = sig_info.sig_pending().has_pending();
        drop(sig_info);
        if has_pending_thread {
            return true;
        }
        // also check shared-pending in sighand
        let shared = self.sighand().shared_pending_signal();
        return !shared.is_empty();
    }

    /// Fast check using PCB flags: whether the current process has any pending signals.
    pub fn has_pending_signal_fast(&self) -> bool {
        self.flags.get().contains(ProcessFlags::HAS_PENDING_SIGNAL)
    }

    /// Checks whether the current process has pending signals that are not
    /// blocked.
    ///
    /// Note: This function is relatively slow and should be used together with
    /// `has_pending_signal_fast`.
    pub fn has_pending_not_masked_signal(&self) -> bool {
        let sig_info = self.sig_info_irqsave();
        let blocked: SigSet = *sig_info.sig_blocked();
        let mut pending: SigSet = sig_info.sig_pending().signal();
        drop(sig_info);
        // Also check shared_pending.
        pending |= self.sighand().shared_pending_signal();
        pending.remove(blocked);
        // log::debug!(
        //     "pending and not masked:{:?}, masked: {:?}",
        //     pending,
        //     blocked
        // );
        let has_not_masked = !pending.is_empty();
        return has_not_masked;
    }

    #[inline(always)]
    pub fn get_robust_list(&self) -> RwLockReadGuard<'_, Option<RobustListHead>> {
        return self.robust_list.read_irqsave();
    }

    #[inline(always)]
    pub fn set_robust_list(&self, new_robust_list: Option<RobustListHead>) {
        *self.robust_list.write_irqsave() = new_robust_list;
    }

    #[inline(always)]
    pub fn alarm_timer_irqsave(&self) -> SpinLockGuard<'_, Option<AlarmTimer>> {
        return self.alarm_timer.lock_irqsave();
    }

    #[inline(always)]
    pub fn itimers_irqsave(&self) -> SpinLockGuard<'_, ProcessItimers> {
        return self.itimers.lock_irqsave();
    }

    pub fn posix_timers_irqsave(&self) -> SpinLockGuard<'_, posix_timer::ProcessPosixTimers> {
        return self.posix_timers.lock_irqsave();
    }

    /// Clean up the timers of the current process/thread.
    ///
    /// References the timer cleanup logic in Linux's `do_exit()`:
    /// ```c
    /// if (group_dead) {
    ///     hrtimer_cancel(&tsk->signal->real_timer);
    ///     exit_itimers(tsk);
    /// }
    /// ```
    ///
    /// In DragonOS, `alarm_timer` is per-PCB, so each exiting thread must cancel
    /// its own alarm timer. `itimers` and `posix_timers` are only cleaned up when
    /// the thread-group leader exits (`group_dead`).
    pub(super) fn exit_timers(&self) {
        // 1. Cancel the current thread's alarm timer.
        if let Some(alarm) = self.alarm_timer.lock_irqsave().take() {
            alarm.cancel();
        }

        let group_dead = self.is_thread_group_leader();
        if group_dead {
            // 2. Cancel ITIMER_REAL.
            if let Some(real_itimer) = self.itimers.lock_irqsave().real.take() {
                real_itimer.timer.cancel();
            }

            // 3. Delete all POSIX interval timers.
            let mut posix_timers = self.posix_timers.lock_irqsave();
            let timer_ids: alloc::vec::Vec<i32> = posix_timers.timer_ids().collect();
            let self_arc = self.self_ref.upgrade();
            if let Some(ref pcb) = self_arc {
                for id in timer_ids {
                    let _ = posix_timers.delete(pcb, id);
                }
            }
        }
    }

    /// Exit fd table when process exit
    pub(super) fn exit_files(&self) {
        // Close the file descriptor table.
        // This is written this way to avoid deadlocks: some inodes need to access
        // the current process's basic info when being closed.
        let mut guard = self.basic.write_irqsave();
        let old = guard.set_fd_table(None);
        drop(guard);
        drop(old)
    }

    pub fn children_read_irqsave(&self) -> RwLockReadGuard<'_, Vec<RawPid>> {
        self.children.read_irqsave()
    }

    pub fn threads_read_irqsave(&self) -> RwLockReadGuard<'_, ThreadInfo> {
        self.thread.read_irqsave()
    }

    pub fn threads_write_irqsave(&self) -> RwLockWriteGuard<'_, ThreadInfo> {
        self.thread.write_irqsave()
    }

    pub fn restart_block(&self) -> SpinLockGuard<'_, Option<RestartBlock>> {
        self.restart_block.lock()
    }

    pub fn set_restart_fn(
        &self,
        restart_block: Option<RestartBlock>,
    ) -> Result<usize, SystemError> {
        *self.restart_block.lock() = restart_block;
        return Err(SystemError::ERESTART_RESTARTBLOCK);
    }

    pub fn parent_pcb(&self) -> Option<Arc<ProcessControlBlock>> {
        self.parent_pcb.read().upgrade()
    }

    pub fn is_exited(&self) -> bool {
        self.sched_info.state().is_exited()
    }

    /// DragonOS sets `ProcessState::Exited` after the task has left user-visible
    /// execution, before `ExitState::Zombie`; it is no longer a live thread-group
    /// member for wait/pidfd readiness.
    pub(crate) fn is_live_thread_group_member(&self) -> bool {
        !self.is_exited() && !self.is_zombie() && !self.is_dead()
    }

    pub fn exit_code(&self) -> Option<usize> {
        self.sched_info.state().exit_code()
    }

    pub fn exit_state(&self) -> ExitState {
        ExitState::from_u8(self.exit_state.load(Ordering::Acquire))
    }

    pub fn is_zombie(&self) -> bool {
        self.exit_state() == ExitState::Zombie
    }

    pub fn is_dead(&self) -> bool {
        self.exit_state() == ExitState::Dead
    }

    pub fn set_exit_state_zombie(&self) {
        self.exit_state
            .store(ExitState::Zombie as u8, Ordering::Release);
    }

    pub fn try_mark_dead_from_zombie(&self) -> bool {
        self.exit_state
            .compare_exchange(
                ExitState::Zombie as u8,
                ExitState::Dead as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub fn mark_exiting(&self) {
        self.flags().insert(ProcessFlags::EXITING);
        fence(Ordering::SeqCst);
    }

    /// Returns the process's namespace proxy.
    pub fn nsproxy(&self) -> Arc<NsProxy> {
        self.nsproxy.load()
    }

    /// Sets the process's namespace proxy.
    ///
    /// ## Parameters
    /// - `nsproxy`: The new namespace proxy.
    pub fn set_nsproxy(&self, nsproxy: Arc<NsProxy>) {
        let _task_guard = self.task_lock.lock_irqsave();
        self.nsproxy.store_deferred(nsproxy);
    }

    pub fn task_cgroup_ref(&self) -> TaskCgroupRef {
        self.task_cgroup.read().clone()
    }

    pub fn task_cgroup_node(&self) -> Arc<CgroupNode> {
        self.task_cgroup.read().node()
    }

    /// Set the cgroup node that this task belongs to.
    ///
    /// # Safety
    ///
    /// The caller must hold `cgroup_accounting_lock` to avoid deadlocks and race
    /// conditions.
    pub fn set_task_cgroup_node(&self, node: Arc<CgroupNode>) {
        // First, use a read lock to get the old node.
        let old = {
            let task_cgroup = self.task_cgroup.read();
            let old = task_cgroup.node();
            if Arc::ptr_eq(&old, &node) {
                return;
            }
            old
        }; // Release the read lock.

        // Perform the migration without holding the task_cgroup lock; the caller
        // must hold cgroup_accounting_lock to guarantee that visible membership
        // and pids charging switch together.
        let pid = self.raw_pid();
        old.remove_task(pid);
        node.add_task(pid);
        CgroupNode::transfer_pids_charge(&old, &node, 1);

        // Use a write lock to update task_cgroup.
        let mut task_cgroup = self.task_cgroup.write();
        *task_cgroup = TaskCgroupRef::new(node);
    }

    /// Set the task's cgroup node for fork only.
    ///
    /// # Safety
    ///
    /// The caller must hold `cgroup_accounting_lock`.
    ///
    /// # Note
    ///
    /// This function only updates the task_cgroup reference and does not call
    /// `add_task()`. `add_task()` will be called subsequently in
    /// `ProcessManager::add_pcb()`.
    pub fn set_task_cgroup_node_for_fork(&self, node: Arc<CgroupNode>) {
        *self.task_cgroup.write() = TaskCgroupRef::new(node);
    }

    pub fn is_thread_group_leader(&self) -> bool {
        self.exit_signal.load(Ordering::SeqCst) >= 0
    }

    pub(crate) fn thread_group_has_live_nonleader_threads(&self) -> bool {
        if !self.is_thread_group_leader() {
            return false;
        }

        self.threads_read_irqsave()
            .group_tasks_clone()
            .into_iter()
            .filter_map(|task| task.upgrade())
            .any(|task| task.is_live_thread_group_member())
    }

    /// Wake all waiters sleeping on this process's `wait_queue`.
    pub fn wake_all_waiters(&self) {
        self.wait_queue
            .wakeup_all(Some(ProcessState::Blocked(true)))
    }
}

impl Drop for ProcessControlBlock {
    fn drop(&mut self) {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        // log::debug!("Drop ProcessControlBlock: pid: {}", self.raw_pid(),);
        self.__exit_signal();
        self.sighand().detach_task_ref();
        // The new ProcFS is dynamic: process directories are created on demand
        // when accessed. Explicit registration/deregistration of processes is no
        // longer needed.
        if let Some(ppcb) = self.parent_pcb.read_irqsave().upgrade() {
            ppcb.children
                .write_irqsave()
                .retain(|pid| *pid != self.raw_pid());
        }

        // log::debug!("Drop pid: {:?}", self.pid());
        drop(irq_guard);
    }
}
