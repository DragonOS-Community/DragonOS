//! Thread-group signal identity and lifecycle, shared only by CLONE_THREAD.
mod cpu_time;
mod exec;
mod job_control;
mod pending;

use core::fmt::Debug;

use alloc::sync::{Arc, Weak};

use system_error::SystemError;

use crate::{
    arch::ipc::signal::{SigSet, Signal},
    ipc::signal_types::{SigCode, SigInfo, SigPending, SignalFlags},
    libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
    libs::spinlock::{SpinLock, SpinLockGuard},
    libs::wait_queue::WaitQueue,
    mm::ucontext::AddressSpace,
    process::{
        pid::{Pid, PidType},
        ProcessControlBlock, RawPid,
    },
};

/// Producer state for the old leader of a non-leader exec transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupExecLeaderPhase {
    Pending,
    Exiting,
    Ready,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupExecCancelResult {
    Canceled,
    Committed,
    NotOwner,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NaturalParentNotifyPhase {
    Idle,
    Pending,
    Done,
}

/// Non-copyable proof that a caller owns the one natural-parent notification
/// transaction for a particular leader.
#[derive(Debug)]
pub struct NaturalParentNotifyToken {
    owner: Weak<ProcessControlBlock>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReapTransition {
    Blocked,
    NotZombie,
    Reportable,
    TraceClaimed,
    Reaped,
}

pub struct ProcessSignalState {
    inner: RwLock<InnerProcessSignalState>,
    group_exec_wait_queue: WaitQueue,
    /// Child state changes belong to the thread group, not its current leader.
    /// Unlike the per-task trapping queue, this survives a leader's exit.
    child_wait_queue: WaitQueue,
    pending: SpinLock<SigPending>,
    cpu_time_wait: Arc<cpu_time::CpuTimeWait>,
    pub(crate) cpu_time_adjustment: SpinLock<crate::sched::cputime::AdjustedCpuTime>,
}

impl Debug for ProcessSignalState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ProcessSignalState").finish()
    }
}

pub struct InnerProcessSignalState {
    /// 进程级信号投递的轮转游标，对应 Linux `signal_struct::curr_target`。
    pub curr_target: Option<Weak<ProcessControlBlock>>,
    pub flags: SignalFlags,
    /// 线程组退出码（仿照 Linux 的 signal_struct::group_exit_code）
    /// 仅当 flags 中包含 GROUP_EXIT 时才有效
    pub group_exit_code: usize,
    /// 最近一次 job-control stop 的信号号，用于 wait(WSTOPPED) 填充 WSTOPSIG。
    pub stop_signal: Signal,
    /// Identity of the current thread-group stop transaction. Zero is reserved
    /// for the initial state; begin/cancel transitions advance it so delayed
    /// ptrace participants cannot complete a later stop.
    group_stop_generation: u64,
    /// Ptrace participants which still have to publish their group-stop. The
    /// existing asynchronous `stop_task()` commits untraced siblings
    /// immediately, so only ptraced siblings need a completion count.
    group_stop_pending_ptraced: usize,
    /// 线程组 exec（de-thread）当前执行者
    pub group_exec_task: Option<Weak<ProcessControlBlock>>,
    /// 线程组 exec（de-thread）等待计数（仿照 Linux 的 signal_struct::notify_count）
    pub group_exec_notify_count: isize,
    /// Stable old leader and its producer state for non-leader exec.
    group_exec_old_leader: Option<Weak<ProcessControlBlock>>,
    group_exec_leader_phase: Option<GroupExecLeaderPhase>,
    /// Monotonically increasing transaction generation. Zero is reserved for
    /// the absence of a per-task token.
    group_exec_generation: u64,
    /// The mm selected by the OOM killer for this thread group.
    ///
    /// The reserve entitlement is not decided by this metadata alone. Callers
    /// must go through `oom::current_is_oom_victim()`, which also verifies that
    /// the task is already fatal/exiting.
    pub oom_tgid: Option<RawPid>,
    pub oom_mm_id: Option<u64>,
    pub oom_mm: Option<Arc<AddressSpace>>,
    pub pids: [Option<Arc<Pid>>; PidType::PIDTYPE_MAX],
}

impl ProcessSignalState {
    pub fn new() -> Arc<Self> {
        Self::try_new().expect("failed to allocate thread-group signal state")
    }

    pub fn try_new() -> Result<Arc<Self>, SystemError> {
        let inner = InnerProcessSignalState::try_default()?;
        Arc::try_new(Self {
            inner: RwLock::new(inner),
            group_exec_wait_queue: WaitQueue::default(),
            child_wait_queue: WaitQueue::default(),
            pending: SpinLock::new(SigPending::default()),
            cpu_time_wait: cpu_time::CpuTimeWait::new(),
            cpu_time_adjustment: SpinLock::new(crate::sched::cputime::AdjustedCpuTime::default()),
        })
        .map_err(|_| SystemError::ENOMEM)
    }

    fn inner(&self) -> RwLockReadGuard<'_, InnerProcessSignalState> {
        self.inner.read_irqsave()
    }

    pub(crate) fn child_wait_queue(&self) -> &WaitQueue {
        &self.child_wait_queue
    }

    fn inner_mut(&self) -> RwLockWriteGuard<'_, InnerProcessSignalState> {
        self.inner.write_irqsave()
    }

    pub fn inner_read(&self) -> RwLockReadGuard<'_, InnerProcessSignalState> {
        self.inner()
    }

    pub fn record_oom_victim_mm(&self, tgid: RawPid, mm: &Arc<AddressSpace>) {
        let mut g = self.inner_mut();
        g.oom_tgid = Some(tgid);
        g.oom_mm_id = Some(mm.id());
        g.oom_mm = Some(mm.clone());
    }

    pub fn clear_oom_mm_if(&self, tgid: RawPid, mm_id: u64) -> bool {
        let mut g = self.inner_mut();
        if g.oom_tgid != Some(tgid) || g.oom_mm_id != Some(mm_id) {
            return false;
        }
        g.oom_tgid = None;
        g.oom_mm_id = None;
        g.oom_mm = None;
        true
    }

    pub fn oom_victim_mm_matches(&self, tgid: RawPid) -> bool {
        let g = self.inner();
        g.oom_tgid == Some(tgid) && g.oom_mm.is_some()
    }

    pub fn curr_target(&self) -> Option<Arc<ProcessControlBlock>> {
        self.inner().curr_target.as_ref().and_then(Weak::upgrade)
    }

    pub fn set_curr_target(&self, task: &Arc<ProcessControlBlock>) {
        self.inner_mut().curr_target = Some(Arc::downgrade(task));
    }

    pub fn clear_curr_target(&self) {
        self.inner_mut().curr_target = None;
    }

    // ===== Signal flags helpers =====
    pub fn flags(&self) -> SignalFlags {
        self.inner().flags
    }

    pub fn flags_contains(&self, flag: SignalFlags) -> bool {
        self.inner().flags.contains(flag)
    }

    pub fn flags_insert(&self, flag: SignalFlags) {
        let mut g = self.inner_mut();
        g.flags.insert(flag);
    }

    pub fn flags_remove(&self, flag: SignalFlags) {
        let mut g = self.inner_mut();
        g.flags.remove(flag);
    }

    pub fn flags_test_and_clear(&self, flag: SignalFlags, clear: bool) -> bool {
        let mut g = self.inner_mut();
        if !g.flags.contains(flag) {
            return false;
        }
        if clear {
            g.flags.remove(flag);
        }
        true
    }

    fn weak_matches(
        weak: &Option<Weak<ProcessControlBlock>>,
        task: &Arc<ProcessControlBlock>,
    ) -> bool {
        weak.as_ref()
            .map(|candidate| Weak::ptr_eq(candidate, &Arc::downgrade(task)))
            .unwrap_or(false)
    }

    fn clear_group_exec_locked(g: &mut InnerProcessSignalState) {
        g.flags.remove(SignalFlags::GROUP_EXEC);
        g.group_exec_task = None;
        g.group_exec_old_leader = None;
        g.group_exec_leader_phase = None;
        g.group_exec_notify_count = 0;
    }

    fn try_reap_natural_child_inner(
        &self,
        candidate: &Arc<ProcessControlBlock>,
        consume: bool,
        token: Option<&NaturalParentNotifyToken>,
    ) -> ReapTransition {
        let g = self.inner_mut();
        if g.flags.contains(SignalFlags::GROUP_EXEC)
            && Self::weak_matches(&g.group_exec_old_leader, candidate)
        {
            return ReapTransition::Blocked;
        }

        if candidate.natural_parent_notify_phase() == NaturalParentNotifyPhase::Pending {
            let owns_notification = token
                .map(|token| Weak::ptr_eq(&token.owner, &Arc::downgrade(candidate)))
                .unwrap_or(false);
            if !owns_notification {
                return ReapTransition::Blocked;
            }
        }

        if !candidate.is_zombie() {
            return ReapTransition::NotZombie;
        }
        if !consume {
            return ReapTransition::Reportable;
        }
        if candidate.try_mark_dead_from_zombie() {
            ReapTransition::Reaped
        } else {
            ReapTransition::NotZombie
        }
    }

    // ===== PIDs helpers =====
    pub fn pid(&self, ty: PidType) -> Option<Arc<Pid>> {
        self.inner().pids[ty as usize].clone()
    }

    pub fn set_pid(&self, ty: PidType, pid: Option<Arc<Pid>>) {
        let mut g = self.inner_mut();
        g.pids[ty as usize] = pid;
    }

    // ===== Refcount helpers =====
}

impl Default for InnerProcessSignalState {
    fn default() -> Self {
        Self::try_default().expect("failed to allocate signal actions")
    }
}

impl InnerProcessSignalState {
    fn try_default() -> Result<Self, SystemError> {
        Ok(Self {
            pids: core::array::from_fn(|_| None),
            group_exit_code: 0,
            stop_signal: Signal::SIGSTOP,
            group_stop_generation: 0,
            group_stop_pending_ptraced: 0,
            curr_target: None,
            flags: SignalFlags::empty(),
            group_exec_task: None,
            group_exec_notify_count: 0,
            group_exec_old_leader: None,
            group_exec_leader_phase: None,
            group_exec_generation: 0,
            oom_tgid: None,
            oom_mm_id: None,
            oom_mm: None,
        })
    }
}
