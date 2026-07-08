use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};

use crate::{
    arch::ipc::signal::{SigSet, Signal},
    driver::tty::tty_core::TtyCore,
    filesystem::vfs::file::FileDescriptorVec,
    ipc::signal_types::{SigInfo, SigPending},
    libs::{rwlock::RwLock, rwsem::RwSem},
    mm::{ucontext::AddressSpace, VirtAddr},
    process::{ProcessControlBlock, ProcessManager, RawPid},
    sched::completion::Completion,
};

#[derive(Debug, Default)]
pub struct CpuItimer {
    pub value: u64,    // remaining time in ns
    pub interval: u64, // interval in ns
    pub is_active: bool,
}

#[derive(Debug, Clone)]
pub struct ProcessItimer {
    pub timer: Arc<crate::time::timer::Timer>,
    pub config: crate::time::syscall::Itimerval,
}

#[derive(Debug, Default)]
pub struct ProcessItimers {
    pub real: Option<ProcessItimer>, // for ITIMER_REAL
    pub virt: CpuItimer,             // for ITIMER_VIRT
    pub prof: CpuItimer,             // for ITIMER_PROF
}

#[derive(Debug)]
pub struct ThreadInfo {
    // Address from userspace to record the thread ID. When this thread exits,
    // the kernel writes 0 to this address to notify the parent process.
    pub(super) clear_child_tid: Option<VirtAddr>,
    pub(super) set_child_tid: Option<VirtAddr>,

    pub(super) vfork_done: Option<Arc<Completion>>,
    /// The thread group leader.
    pub(super) group_leader: Weak<ProcessControlBlock>,

    /// When the current thread is the group leader, this field stores the
    /// PCBs of all threads in the group.
    pub(super) group_tasks: Vec<Weak<ProcessControlBlock>>,
}

impl Default for ThreadInfo {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadInfo {
    pub fn new() -> Self {
        Self {
            clear_child_tid: None,
            set_child_tid: None,
            vfork_done: None,
            group_leader: Weak::default(),
            group_tasks: Vec::new(),
        }
    }

    pub fn group_leader(&self) -> Option<Arc<ProcessControlBlock>> {
        return self.group_leader.upgrade();
    }

    /// Returns the list of threads in the thread group excluding the leader
    /// (maintained by the group leader).
    ///
    /// Note: The list is stored as `Weak` references. Callers should `upgrade()`
    /// each entry before use and handle upgrade failures.
    pub fn group_tasks_clone(&self) -> Vec<Weak<ProcessControlBlock>> {
        self.group_tasks.clone()
    }

    pub fn thread_group_empty(&self) -> bool {
        let group_leader = self.group_leader();
        if let Some(leader) = group_leader {
            if Arc::ptr_eq(&leader, &ProcessManager::current_pcb()) {
                if self.group_tasks.is_empty() {
                    return true;
                }
                // Only return false when there are “live threads” in the group
                for weak in &self.group_tasks {
                    if let Some(task) = weak.upgrade() {
                        if Arc::ptr_eq(&task, &ProcessManager::current_pcb()) {
                            continue;
                        }
                        if !task.is_exited() && !task.is_dead() && !task.is_zombie() {
                            return false;
                        }
                    }
                }
                return true;
            }
            return false;
        }
        return true;
    }
}

/// Basic information about a process.
///
/// This struct holds process metadata that rarely changes during the process
/// lifetime.
#[derive(Debug)]
pub struct ProcessBasicInfo {
    /// PID of the current process's parent.
    pub(super) ppid: RawPid,
    /// Process name.
    name: String,

    /// Current working directory of the process.
    cwd: String,

    /// User address space.
    user_vm: Option<Arc<AddressSpace>>,

    /// File descriptor table.
    fd_table: Option<Arc<RwSem<FileDescriptorVec>>>,
}

impl ProcessBasicInfo {
    #[inline(never)]
    pub fn new(
        ppid: RawPid,
        name: String,
        cwd: String,
        user_vm: Option<Arc<AddressSpace>>,
    ) -> RwLock<Self> {
        let fd_table = Arc::new(RwSem::new(FileDescriptorVec::new()));
        return RwLock::new(Self {
            ppid,
            name,
            cwd,
            user_vm,
            fd_table: Some(fd_table),
        });
    }

    pub fn ppid(&self) -> RawPid {
        return self.ppid;
    }

    pub fn name(&self) -> &str {
        return &self.name;
    }

    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    pub fn cwd(&self) -> String {
        return self.cwd.clone();
    }
    pub fn set_cwd(&mut self, path: String) {
        return self.cwd = path;
    }

    pub fn user_vm(&self) -> Option<Arc<AddressSpace>> {
        return self.user_vm.clone();
    }

    pub unsafe fn set_user_vm(&mut self, user_vm: Option<Arc<AddressSpace>>) {
        self.user_vm = user_vm;
    }

    /// Replace the task's user address space and return the old one without
    /// dropping it while this lock is still held. The caller must preserve the
    /// active_cpus and TLB-state ordering required by the mm switch/exit path.
    pub unsafe fn replace_user_vm(
        &mut self,
        user_vm: Option<Arc<AddressSpace>>,
    ) -> Option<Arc<AddressSpace>> {
        let old = self.user_vm.take();
        self.user_vm = user_vm;
        old
    }

    pub fn try_fd_table(&self) -> Option<Arc<RwSem<FileDescriptorVec>>> {
        return self.fd_table.clone();
    }

    #[inline]
    pub fn fd_table_is_shared(&self) -> bool {
        self.fd_table
            .as_ref()
            .map(|t| Arc::strong_count(t) > 1)
            .unwrap_or(false)
    }

    pub fn set_fd_table(
        &mut self,
        fd_table: Option<Arc<RwSem<FileDescriptorVec>>>,
    ) -> Option<Arc<RwSem<FileDescriptorVec>>> {
        let old = self.fd_table.take();
        self.fd_table = fd_table;
        return old;
    }
}

#[derive(Debug)]

pub struct ProcessSignalInfo {
    // Signals currently blocked by this process.
    sig_blocked: SigSet,
    // Original blocked mask used by sigtimedwait while it temporarily unblocks
    // the waited signal set.
    real_blocked: SigSet,
    // Saved old signal mask, used for restoration.
    saved_sigmask: SigSet,
    // sig_pending stores the signals pending for the current thread.
    sig_pending: SigPending,

    // The tty associated with the current process.
    tty: Option<Arc<TtyCore>>,
    has_child_subreaper: bool,

    /// Marks whether the current process is a “child subreaper.”
    ///
    /// TODO: Implement the prctl interface for setting this flag.
    is_child_subreaper: bool,

    /// boolean value for session group leader
    pub is_session_leader: bool,

    /// OOM killer score adjustment exposed through `/proc/[pid]/oom_score_adj`.
    oom_score_adj: i16,
    /// Minimum oom_score_adj an unprivileged writer may set.
    ///
    /// Linux updates this value only from CAP_SYS_RESOURCE writes. The default
    /// is 0, so an unprivileged process cannot make itself more protected than
    /// the initial state.
    oom_score_adj_min: i16,
}

impl ProcessSignalInfo {
    pub fn sig_blocked(&self) -> &SigSet {
        &self.sig_blocked
    }

    pub fn sig_pending(&self) -> &SigPending {
        &self.sig_pending
    }

    pub fn sig_pending_mut(&mut self) -> &mut SigPending {
        &mut self.sig_pending
    }

    pub fn sig_block_mut(&mut self) -> &mut SigSet {
        &mut self.sig_blocked
    }

    pub fn real_blocked(&self) -> &SigSet {
        &self.real_blocked
    }

    pub fn real_blocked_mut(&mut self) -> &mut SigSet {
        &mut self.real_blocked
    }

    pub fn saved_sigmask(&self) -> &SigSet {
        &self.saved_sigmask
    }

    pub fn saved_sigmask_mut(&mut self) -> &mut SigSet {
        &mut self.saved_sigmask
    }

    pub fn tty(&self) -> Option<Arc<TtyCore>> {
        self.tty.clone()
    }

    pub fn set_tty(&mut self, tty: Option<Arc<TtyCore>>) {
        self.tty = tty;
    }

    /// Dequeue the next signal to be processed from the current thread's
    /// pending set.
    ///
    /// ## Parameters
    ///
    /// - `sig_mask`: Signals to be ignored (masked out).
    pub fn dequeue_thread_signal(&mut self, sig_mask: &SigSet) -> (Signal, Option<SigInfo>) {
        self.sig_pending.dequeue_signal(sig_mask)
    }

    pub fn has_child_subreaper(&self) -> bool {
        self.has_child_subreaper
    }

    pub fn set_has_child_subreaper(&mut self, has_child_subreaper: bool) {
        self.has_child_subreaper = has_child_subreaper;
    }

    pub fn is_child_subreaper(&self) -> bool {
        self.is_child_subreaper
    }

    pub fn set_is_child_subreaper(&mut self, is_child_subreaper: bool) {
        self.is_child_subreaper = is_child_subreaper;
    }

    pub fn oom_score_adj(&self) -> i16 {
        self.oom_score_adj
    }

    pub fn set_oom_score_adj(&mut self, oom_score_adj: i16) {
        self.oom_score_adj = oom_score_adj;
    }

    pub fn oom_score_adj_min(&self) -> i16 {
        self.oom_score_adj_min
    }

    pub fn set_oom_score_adj_min(&mut self, oom_score_adj_min: i16) {
        self.oom_score_adj_min = oom_score_adj_min;
    }
}

impl Default for ProcessSignalInfo {
    fn default() -> Self {
        Self {
            sig_blocked: SigSet::empty(),
            real_blocked: SigSet::empty(),
            saved_sigmask: SigSet::empty(),
            sig_pending: SigPending::default(),
            tty: None,
            has_child_subreaper: false,
            is_child_subreaper: false,
            is_session_leader: false,
            oom_score_adj: 0,
            oom_score_adj_min: 0,
        }
    }
}
