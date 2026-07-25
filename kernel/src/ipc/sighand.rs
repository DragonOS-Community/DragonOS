use core::{fmt::Debug, sync::atomic::compiler_fence};

use alloc::sync::{Arc, Weak};

use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::ipc::signal::{SigFlags, SigSet, Signal, MAX_SIG_NUM},
    filesystem::epoll::event_poll::LockedEPItemLinkedList,
    ipc::signal_types::{SaHandlerType, SigCode, SigInfo, SigPending, SigactionType, SignalFlags},
    libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
    libs::wait_queue::WaitQueue,
    mm::ucontext::AddressSpace,
    process::{
        pid::{Pid, PidType},
        ProcessControlBlock, ProcessManager, RawPid,
    },
};

use super::signal_types::Sigaction;

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
    Reaped,
}

pub struct SigHand {
    inner: RwLock<InnerSigHand>,
    group_exec_wait_queue: WaitQueue,
    /// signalfd 共享等待队列，对标 Linux `sighand_struct::signalfd_wqh`。
    ///
    /// 信号投递路径（包括 hardirq 上下文）调用 `signalfd_wqh.wakeup_all()`
    /// 唤醒所有阻塞在 signalfd `read()` 的线程。
    /// `WaitQueue` 内部使用 `SpinLock` + `lock_irqsave`，hardirq 安全。
    signalfd_wqh: WaitQueue,
    /// signalfd 的 epoll 通知列表。
    ///
    /// 所有注册在此进程 signalfd 上的 EPollItem 都会被收集到这里。
    /// 信号投递路径（包括 hardirq）直接对此列表调用 `wakeup_epoll`，
    /// 避免遍历 fd_table（涉及 RwLock/RwSem，在 hardirq 中不安全）。
    /// 使用 irqsave SpinLock，hardirq 安全。
    signalfd_epitems: LockedEPItemLinkedList,
}

impl Debug for SigHand {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SigHand").finish()
    }
}

pub struct InnerSigHand {
    pub handlers: Vec<Sigaction>,
    /// 当前线程所属进程要处理的信号
    pub shared_pending: SigPending,
    /// 进程级信号投递的轮转游标，对应 Linux `signal_struct::curr_target`。
    pub curr_target: Option<Weak<ProcessControlBlock>>,
    pub flags: SignalFlags,
    /// 线程组退出码（仿照 Linux 的 signal_struct::group_exit_code）
    /// 仅当 flags 中包含 GROUP_EXIT 时才有效
    pub group_exit_code: usize,
    /// 最近一次 job-control stop 的信号号，用于 wait(WSTOPPED) 填充 WSTOPSIG。
    pub stop_signal: Signal,
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
    /// 在 sighand 上维护的引用计数（与 Linux 一致的布局位置）
    pub cnt: i64,
}

impl SigHand {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(InnerSigHand::default()),
            group_exec_wait_queue: WaitQueue::default(),
            signalfd_wqh: WaitQueue::default(),
            signalfd_epitems: LockedEPItemLinkedList::default(),
        })
    }

    fn inner(&self) -> RwLockReadGuard<'_, InnerSigHand> {
        self.inner.read_irqsave()
    }

    fn inner_mut(&self) -> RwLockWriteGuard<'_, InnerSigHand> {
        self.inner.write_irqsave()
    }

    pub fn inner_read(&self) -> RwLockReadGuard<'_, InnerSigHand> {
        self.inner()
    }

    fn group_exec_wait_queue(&self) -> &WaitQueue {
        &self.group_exec_wait_queue
    }

    /// 获取 signalfd 共享等待队列引用。
    ///
    /// signalfd 的 `read()` 路径在此队列上注册等待者，
    /// 信号投递路径通过 `wakeup_all()` 唤醒它们。
    pub fn signalfd_wqh(&self) -> &WaitQueue {
        &self.signalfd_wqh
    }

    /// 获取 signalfd 的 epoll 通知列表引用。
    ///
    /// signalfd 的 `add_epitem` 将 EPollItem 同时注册到此列表中，
    /// 信号投递路径通过 `wakeup_epoll(&signalfd_epitems, ...)` 直接通知 epoll，
    /// 避免在 hardirq 中遍历 fd_table。
    pub fn signalfd_epitems(&self) -> &LockedEPItemLinkedList {
        &self.signalfd_epitems
    }

    pub fn attach_task_ref(&self) {
        let mut g = self.inner_mut();
        g.cnt += 1;
    }

    pub fn detach_task_ref(&self) {
        let mut g = self.inner_mut();
        assert!(g.cnt > 0, "SigHand::detach_task_ref underflow");
        g.cnt -= 1;
    }

    pub fn wait_group_exec_event_interruptible<F, B>(
        &self,
        cond: F,
        before_sleep: Option<B>,
    ) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
        B: FnMut(),
    {
        self.group_exec_wait_queue()
            .wait_event_interruptible(cond, before_sleep)
    }

    pub fn wait_group_exec_event_killable<F, B>(
        &self,
        cond: F,
        before_sleep: Option<B>,
    ) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
        B: FnMut(),
    {
        self.group_exec_wait_queue()
            .wait_event_killable(cond, before_sleep)
    }

    pub fn wait_group_exec_event_uninterruptible<F, B>(
        &self,
        cond: F,
        before_sleep: Option<B>,
    ) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
        B: FnMut(),
    {
        self.group_exec_wait_queue()
            .wait_event_uninterruptible(cond, before_sleep)
    }

    pub fn reset_handlers(&self) {
        self.inner_mut().handlers = default_sighandlers();
    }

    pub fn handler(&self, sig: Signal) -> Option<Sigaction> {
        self.inner().handlers.get(Self::sig2idx(sig)).cloned()
    }

    pub fn set_handler(&self, sig: Signal, act: Sigaction) {
        if let Some(h) = self.inner_mut().handlers.get_mut(Self::sig2idx(sig)) {
            *h = act;
        }
    }

    fn sig2idx(sig: Signal) -> usize {
        sig as usize - 1
    }

    pub fn copy_handlers_from(&self, other: &Arc<SigHand>) {
        let other_guard = other.inner();
        let mut self_guard = self.inner_mut();
        self_guard.handlers = other_guard.handlers.clone();
    }

    pub fn copy_process_state_from(&self, other: &Arc<SigHand>) {
        let other_guard = other.inner();
        let mut self_guard = self.inner_mut();
        self_guard.flags = other_guard.flags;
        // Group-exec is a transaction owned by the original shared sighand;
        // copying only its flag would create an active state without owner,
        // generation, phase, or pending tokens.
        self_guard.flags.remove(SignalFlags::GROUP_EXEC);
        self_guard.group_exit_code = other_guard.group_exit_code;
        self_guard.pids = other_guard.pids.clone();
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

    // ===== Shared pending helpers =====
    pub fn shared_pending_signal(&self) -> SigSet {
        let g = self.inner();
        g.shared_pending.signal()
    }

    pub fn shared_pending_flush_by_mask(&self, mask: &SigSet) {
        let mut g = self.inner_mut();
        g.shared_pending.flush_by_mask(mask);
    }

    pub fn shared_pending_queue_has(&self, sig: Signal) -> bool {
        let g = self.inner();
        g.shared_pending.queue().find(sig).0.is_some()
    }

    /// 查找并判断 shared pending 队列中是否已存在指定 timerid 的 POSIX timer 信号。
    pub fn shared_pending_posix_timer_exists(&self, sig: Signal, timerid: i32) -> bool {
        let mut g = self.inner_mut();
        for info in g.shared_pending.queue_mut().q.iter_mut() {
            // bump(0) 作为“匹配探测”，不会改变值
            if info.is_signal(sig)
                && info.sig_code() == SigCode::Timer
                && info.bump_posix_timer_overrun(timerid, 0)
            {
                return true;
            }
        }
        false
    }

    /// 若 shared pending 中已存在该 timer 的信号，则将其 si_overrun 增加 bump，并返回 true。
    pub fn shared_pending_posix_timer_bump_overrun(
        &self,
        sig: Signal,
        timerid: i32,
        bump: i32,
    ) -> bool {
        let mut g = self.inner_mut();
        for info in g.shared_pending.queue_mut().q.iter_mut() {
            if info.is_signal(sig)
                && info.sig_code() == SigCode::Timer
                && info.bump_posix_timer_overrun(timerid, bump)
            {
                return true;
            }
        }
        false
    }

    /// 将 shared pending 中属于该 timer 的信号的 si_overrun 重置为 0（若找到则返回 true）。
    pub fn shared_pending_posix_timer_reset_overrun(&self, sig: Signal, timerid: i32) -> bool {
        let mut g = self.inner_mut();
        for info in g.shared_pending.queue_mut().q.iter_mut() {
            if info.is_signal(sig)
                && info.sig_code() == SigCode::Timer
                && info.reset_posix_timer_overrun(timerid)
            {
                return true;
            }
        }
        false
    }

    pub fn shared_pending_dequeue(&self, sig_mask: &SigSet) -> (Signal, Option<SigInfo>) {
        let mut g = self.inner_mut();
        g.shared_pending.dequeue_signal(sig_mask)
    }

    /// 向 shared_pending 队列添加信号
    pub fn shared_pending_push(&self, sig: Signal, info: SigInfo) {
        let mut g = self.inner_mut();
        g.shared_pending.queue_mut().q.push(info);
        g.shared_pending.signal_mut().insert(sig.into());
    }

    /// 向 shared_pending 的 signal 位图中添加信号（不添加 siginfo）
    pub fn shared_pending_signal_insert(&self, sig: Signal) {
        let mut g = self.inner_mut();
        g.shared_pending.signal_mut().insert(sig.into());
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

    /// Apply a job-control stop and publish its wait event as one state
    /// transition. The callback runs while the shared sighand is locked, so a
    /// concurrent SIGCONT cannot wake the group between the scheduler-state
    /// update and the persistent stop-state publication.
    ///
    /// Returns whether this completed a fresh group stop which should be
    /// reported to the parent. Repeated stop signals keep the existing event.
    pub fn transition_group_stop<F>(&self, sig: Signal, stop_group: F) -> bool
    where
        F: FnOnce() -> bool,
    {
        let mut g = self.inner_mut();
        if g.flags.contains(SignalFlags::GROUP_EXIT) || !stop_group() {
            return false;
        }
        if g.flags.contains(SignalFlags::STOP_STOPPED) {
            return false;
        }

        g.stop_signal = sig;
        g.flags.remove(SignalFlags::STOP_MASK);
        g.flags
            .insert(SignalFlags::STOP_STOPPED | SignalFlags::CLD_STOPPED);
        true
    }

    /// Continue a job-control-stopped group as one transition. The callback is
    /// run whenever group exit has not started because SIGCONT resumes stopped
    /// tasks even when no parent notification is pending. A continued event is
    /// generated only for a completed group stop, matching Linux's
    /// SIGNAL_STOP_STOPPED test.
    pub fn transition_group_continue<F>(&self, continue_group: F) -> bool
    where
        F: FnOnce(),
    {
        let mut g = self.inner_mut();
        if g.flags.contains(SignalFlags::GROUP_EXIT) {
            return false;
        }
        let was_stopped = g.flags.contains(SignalFlags::STOP_STOPPED);

        continue_group();
        if was_stopped {
            g.flags.remove(SignalFlags::STOP_MASK);
            g.flags
                .insert(SignalFlags::STOP_CONTINUED | SignalFlags::CLD_CONTINUED);
        }
        was_stopped
    }

    /// Observe the persistent natural-child group-stop event. The stop state,
    /// stop signal, and optional consumption are protected by one lock, as are
    /// Linux's SIGNAL_STOP_STOPPED and group_exit_code checks.
    pub fn group_stop_event(&self, consume: bool) -> Option<Signal> {
        let mut g = self.inner_mut();
        if !g.flags.contains(SignalFlags::STOP_STOPPED)
            || !g.flags.contains(SignalFlags::CLD_STOPPED)
        {
            return None;
        }

        let sig = g.stop_signal;
        if consume {
            g.flags.remove(SignalFlags::CLD_STOPPED);
        }
        Some(sig)
    }

    /// Observe a ptrace stop without imposing the natural-child
    /// STOP_STOPPED requirement. The scheduler-state recheck, event code, and
    /// optional consumption stay in the same sighand critical section.
    pub fn ptrace_stop_event<F>(&self, consume: bool, is_stopped: F) -> Option<Signal>
    where
        F: FnOnce() -> bool,
    {
        let mut g = self.inner_mut();
        if !is_stopped() || !g.flags.contains(SignalFlags::CLD_STOPPED) {
            return None;
        }

        let sig = g.stop_signal;
        if consume {
            g.flags.remove(SignalFlags::CLD_STOPPED);
        }
        Some(sig)
    }

    pub fn stop_signal(&self) -> Signal {
        self.inner().stop_signal
    }

    pub fn set_stop_signal(&self, sig: Signal) {
        let mut g = self.inner_mut();
        g.stop_signal = sig;
    }

    /// Start group exec and collect the transaction's ordinary sibling tokens
    /// under the same lock that completion uses.
    ///
    /// The callback may acquire thread-info locks, establishing the fixed
    /// `SigHand -> thread-info` order. It must assign `generation` to every
    /// identity-incomplete ordinary sibling and return that count.
    pub fn start_group_exec_transaction<F, R>(
        &self,
        owner: &Arc<ProcessControlBlock>,
        old_leader: Option<&Arc<ProcessControlBlock>>,
        collect: F,
    ) -> Result<R, SystemError>
    where
        F: FnOnce(u64) -> (R, usize),
    {
        let mut g = self.inner_mut();
        if g.flags.contains(SignalFlags::GROUP_EXIT) || g.flags.contains(SignalFlags::GROUP_EXEC) {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        debug_assert!(old_leader
            .map(|leader| !Arc::ptr_eq(leader, owner))
            .unwrap_or(true));
        if old_leader
            .map(|leader| {
                leader.is_dead() || (leader.exit_notify_complete() && !leader.is_zombie())
            })
            .unwrap_or(false)
        {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        let mut generation = g.group_exec_generation.wrapping_add(1);
        if generation == 0 {
            generation = 1;
        }
        let (result, pending) = collect(generation);

        g.flags.insert(SignalFlags::GROUP_EXEC);
        g.group_exec_task = Some(Arc::downgrade(owner));
        g.group_exec_old_leader = old_leader.map(Arc::downgrade);
        g.group_exec_leader_phase = old_leader.map(|leader| {
            if leader.exit_notify_complete() {
                GroupExecLeaderPhase::Ready
            } else {
                GroupExecLeaderPhase::Pending
            }
        });
        g.group_exec_generation = generation;
        g.group_exec_notify_count = pending as isize;
        let leader_ready = g.group_exec_leader_phase == Some(GroupExecLeaderPhase::Ready);
        drop(g);

        if leader_ready {
            self.group_exec_wait_queue().wakeup_all(None);
        }
        Ok(result)
    }

    /// Finish only the caller's transaction. A non-leader exec can finish only
    /// after the old-leader producer reached Ready.
    pub fn finish_group_exec_owned(&self, owner: &Arc<ProcessControlBlock>) -> bool {
        let mut g = self.inner_mut();
        if !Self::weak_matches(&g.group_exec_task, owner)
            || g.group_exec_notify_count != 0
            || matches!(
                g.group_exec_leader_phase,
                Some(GroupExecLeaderPhase::Pending | GroupExecLeaderPhase::Exiting)
            )
        {
            return false;
        }
        Self::clear_group_exec_locked(&mut g);
        drop(g);
        self.group_exec_wait_queue().wakeup_all(None);
        true
    }

    /// Cancel before the old leader commits its dedicated exit. Once Exiting
    /// has been claimed, the identity handoff must complete uninterruptibly.
    pub fn try_cancel_group_exec(&self, owner: &Arc<ProcessControlBlock>) -> GroupExecCancelResult {
        let mut g = self.inner_mut();
        if !g.flags.contains(SignalFlags::GROUP_EXEC)
            || !Self::weak_matches(&g.group_exec_task, owner)
        {
            return GroupExecCancelResult::NotOwner;
        }
        if matches!(
            g.group_exec_leader_phase,
            Some(GroupExecLeaderPhase::Exiting | GroupExecLeaderPhase::Ready)
        ) {
            return GroupExecCancelResult::Committed;
        }

        Self::clear_group_exec_locked(&mut g);
        drop(g);
        self.group_exec_wait_queue().wakeup_all(None);
        GroupExecCancelResult::Canceled
    }

    /// Atomically claim the old leader's dedicated producer path.
    pub fn claim_group_exec_leader_exit(&self, candidate: &Arc<ProcessControlBlock>) -> bool {
        let mut g = self.inner_mut();
        if !g.flags.contains(SignalFlags::GROUP_EXEC)
            || !Self::weak_matches(&g.group_exec_old_leader, candidate)
            || g.group_exec_leader_phase != Some(GroupExecLeaderPhase::Pending)
        {
            return false;
        }
        g.group_exec_leader_phase = Some(GroupExecLeaderPhase::Exiting);
        true
    }

    /// Publish producer completion before waking the exec waiter. This also
    /// covers a leader that entered ordinary exit before group exec started.
    pub fn complete_group_exec_leader_exit(&self, candidate: &Arc<ProcessControlBlock>) -> bool {
        if !candidate.exit_notify_complete() {
            return false;
        }
        let mut g = self.inner_mut();
        if !g.flags.contains(SignalFlags::GROUP_EXEC)
            || !Self::weak_matches(&g.group_exec_old_leader, candidate)
            || !matches!(
                g.group_exec_leader_phase,
                Some(GroupExecLeaderPhase::Pending | GroupExecLeaderPhase::Exiting)
            )
        {
            return false;
        }
        g.group_exec_leader_phase = Some(GroupExecLeaderPhase::Ready);
        drop(g);
        self.group_exec_wait_queue().wakeup_all(None);
        true
    }

    pub fn group_exec_leader_phase(
        &self,
        owner: &Arc<ProcessControlBlock>,
    ) -> Option<GroupExecLeaderPhase> {
        let g = self.inner();
        Self::weak_matches(&g.group_exec_task, owner).then_some(g.group_exec_leader_phase)?
    }

    pub fn group_exec_pending_complete(&self, owner: &Arc<ProcessControlBlock>) -> bool {
        let g = self.inner();
        Self::weak_matches(&g.group_exec_task, owner) && g.group_exec_notify_count == 0
    }

    pub fn group_exec_handoff_ready(&self, owner: &Arc<ProcessControlBlock>) -> bool {
        let g = self.inner();
        Self::weak_matches(&g.group_exec_task, owner)
            && g.group_exec_notify_count == 0
            && matches!(
                g.group_exec_leader_phase,
                None | Some(GroupExecLeaderPhase::Ready)
            )
    }

    pub fn group_exec_committed(&self, owner: &Arc<ProcessControlBlock>) -> bool {
        let g = self.inner();
        Self::weak_matches(&g.group_exec_task, owner)
            && matches!(
                g.group_exec_leader_phase,
                Some(GroupExecLeaderPhase::Exiting | GroupExecLeaderPhase::Ready)
            )
    }

    /// Complete one ordinary sibling's identity-unhash token in O(1).
    pub fn complete_group_exec_task(&self, candidate: &Arc<ProcessControlBlock>) -> bool {
        if !candidate.identity_unhash_complete() {
            return false;
        }
        let mut g = self.inner_mut();
        let generation = candidate.take_group_exec_generation();
        if generation == 0
            || !g.flags.contains(SignalFlags::GROUP_EXEC)
            || generation != g.group_exec_generation
        {
            return false;
        }
        assert!(
            g.group_exec_notify_count > 0,
            "group-exec pending count underflow"
        );
        g.group_exec_notify_count -= 1;
        let ready = g.group_exec_notify_count == 0;
        drop(g);
        if ready {
            self.group_exec_wait_queue().wakeup_all(None);
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

    fn clear_group_exec_locked(g: &mut InnerSigHand) {
        g.flags.remove(SignalFlags::GROUP_EXEC);
        g.group_exec_task = None;
        g.group_exec_old_leader = None;
        g.group_exec_leader_phase = None;
        g.group_exec_notify_count = 0;
    }

    /// 在与 GROUP_EXEC/GROUP_EXIT 相同的锁下执行关键区，避免并发插入线程组。
    pub fn with_group_exec_check<F, R>(&self, f: F) -> Result<R, SystemError>
    where
        F: FnOnce() -> R,
    {
        let g = self.inner_mut();
        if g.flags.contains(SignalFlags::GROUP_EXIT) || g.flags.contains(SignalFlags::GROUP_EXEC) {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        let ret = f();
        drop(g);
        Ok(ret)
    }

    /// 获取当前 exec 线程（去线程化执行者）。
    pub fn group_exec_task(&self) -> Option<Arc<ProcessControlBlock>> {
        self.inner().group_exec_task.as_ref()?.upgrade()
    }

    /// Claim the leader's natural-parent notification responsibility while
    /// holding the same lock used by group-exec/reap arbitration. `eligible`
    /// may acquire the leader thread-info lock (`SigHand -> thread-info`).
    pub fn try_claim_natural_parent_notify<F>(
        &self,
        candidate: &Arc<ProcessControlBlock>,
        eligible: F,
    ) -> Option<NaturalParentNotifyToken>
    where
        F: FnOnce() -> bool,
    {
        self.try_claim_natural_parent_notify_with(candidate, || ((), eligible()))
            .1
    }

    /// Variant for the last-sibling unhash path. `transition` always runs
    /// while the sighand lock is held, so it can remove the sibling under the
    /// nested leader thread-info lock and return whether that removal made a
    /// Zombie leader eligible for natural-parent notification.
    pub fn try_claim_natural_parent_notify_with<F, R>(
        &self,
        candidate: &Arc<ProcessControlBlock>,
        transition: F,
    ) -> (R, Option<NaturalParentNotifyToken>)
    where
        F: FnOnce() -> (R, bool),
    {
        let g = self.inner_mut();
        let (result, eligible) = transition();
        if !candidate.is_thread_group_leader()
            || (g.flags.contains(SignalFlags::GROUP_EXEC)
                && Self::weak_matches(&g.group_exec_old_leader, candidate))
            || !eligible
        {
            return (result, None);
        }
        let owner = Arc::downgrade(candidate);
        let token = candidate
            .try_claim_natural_parent_notify()
            .then_some(NaturalParentNotifyToken { owner });
        (result, token)
    }

    pub fn complete_natural_parent_notify(&self, token: NaturalParentNotifyToken) -> bool {
        token
            .owner
            .upgrade()
            .map(|owner| owner.complete_natural_parent_notify())
            .unwrap_or(false)
    }

    /// Stable report/reap decision for natural-parent wait and autoreap.
    pub fn try_reap_natural_child(
        &self,
        candidate: &Arc<ProcessControlBlock>,
        consume: bool,
    ) -> ReapTransition {
        self.try_reap_natural_child_inner(candidate, consume, None)
    }

    /// Read-only fast probe used by wait scans before attempting a consuming
    /// transition. The consuming helper rechecks these barriers under the
    /// write lock, so a concurrent transaction cannot slip through a TOCTOU
    /// window.
    pub fn natural_reap_blocked(&self, candidate: &Arc<ProcessControlBlock>) -> bool {
        let g = self.inner();
        (g.flags.contains(SignalFlags::GROUP_EXEC)
            && Self::weak_matches(&g.group_exec_old_leader, candidate))
            || candidate.natural_parent_notify_phase() == NaturalParentNotifyPhase::Pending
    }

    /// Stable ptrace report/reap decision. Group-exec arbitration and the
    /// optional Zombie -> Dead transition share one SigHand critical section.
    pub fn try_reap_ptraced_child(
        &self,
        candidate: &Arc<ProcessControlBlock>,
        consume: bool,
    ) -> ReapTransition {
        let g = self.inner_mut();
        if g.flags.contains(SignalFlags::GROUP_EXEC)
            && Self::weak_matches(&g.group_exec_old_leader, candidate)
        {
            return ReapTransition::Blocked;
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

    /// Autoreap used by the unique natural-parent notification owner. The
    /// token bypasses only its own Pending barrier, never a group-exec barrier.
    pub fn try_reap_natural_child_as_notify_owner(
        &self,
        candidate: &Arc<ProcessControlBlock>,
        token: &NaturalParentNotifyToken,
    ) -> ReapTransition {
        self.try_reap_natural_child_inner(candidate, true, Some(token))
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

    pub fn reap_blocked_by_group_exec(&self, candidate: &Arc<ProcessControlBlock>) -> bool {
        let g = self.inner();
        g.flags.contains(SignalFlags::GROUP_EXEC)
            && Self::weak_matches(&g.group_exec_old_leader, candidate)
    }

    /// 若当前线程组已经处于 group-exit 状态，则返回统一的退出码；否则返回 None
    pub fn group_exit_code_if_set(&self) -> Option<usize> {
        let g = self.inner();
        if g.flags.contains(SignalFlags::GROUP_EXIT) {
            Some(g.group_exit_code)
        } else {
            None
        }
    }

    /// 启动线程组退出：
    /// - 若此前尚未标记 GROUP_EXIT，则设置标志与退出码，并返回本次传入的退出码
    /// - 若已经有线程设置了 GROUP_EXIT，则直接返回已存在的 group_exit_code
    pub fn start_group_exit(&self, exit_code: usize) -> usize {
        let mut g = self.inner_mut();
        if g.flags.contains(SignalFlags::GROUP_EXIT) {
            g.group_exit_code
        } else {
            // Linux do_group_exit() replaces signal->flags with
            // SIGNAL_GROUP_EXIT, discarding all job-control wait state.
            g.flags.remove(SignalFlags::STOP_MASK);
            g.flags.insert(SignalFlags::GROUP_EXIT);
            g.group_exit_code = exit_code;
            exit_code
        }
    }

    /// Initiates thread group exit triggered by a fatal signal.
    ///
    /// In Linux, the fatal group-exit branch in `complete_signal()` overwrites
    /// stop/job-control state. DragonOS currently lacks a full jobctl structure,
    /// but the stopped/continued state visible to wait is stored in
    /// `SignalFlags::STOP_MASK`; we clear it here before setting GROUP_EXIT to
    /// prevent a soon-to-be-killed stopped thread group from exposing stale
    /// stop/continue events.
    ///
    /// Returns true only for the caller that actually transitions the group
    /// into GROUP_EXIT.
    pub fn start_group_exit_for_fatal_signal(&self, exit_code: usize) -> bool {
        let mut g = self.inner_mut();
        if g.flags.contains(SignalFlags::GROUP_EXIT) {
            false
        } else {
            g.flags.remove(SignalFlags::STOP_MASK);
            g.flags.insert(SignalFlags::GROUP_EXIT);
            g.group_exit_code = exit_code;
            true
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
    pub fn load_count(&self) -> i64 {
        self.inner().cnt
    }

    pub fn is_shared(&self) -> bool {
        self.load_count() > 1
    }
}

impl Default for InnerSigHand {
    fn default() -> Self {
        Self {
            handlers: default_sighandlers(),
            pids: core::array::from_fn(|_| None),
            shared_pending: SigPending::default(),
            curr_target: None,
            flags: SignalFlags::empty(),
            group_exit_code: 0,
            stop_signal: Signal::SIGSTOP,
            group_exec_task: None,
            group_exec_notify_count: 0,
            group_exec_old_leader: None,
            group_exec_leader_phase: None,
            group_exec_generation: 0,
            oom_tgid: None,
            oom_mm_id: None,
            oom_mm: None,
            cnt: 0,
        }
    }
}

fn default_sighandlers() -> Vec<Sigaction> {
    let mut r = vec![Sigaction::default(); MAX_SIG_NUM];
    let mut sig_ign = Sigaction::default();
    // 收到忽略的信号，重启系统调用
    // Linux ignores SIGURG/SIGWINCH by default; SIGCHLD is also ignored by default,
    // but the handler must remain SIG_DFL to distinguish default ignore from explicit SIG_IGN.
    sig_ign.set_action(SigactionType::SaHandler(SaHandlerType::Ignore));
    sig_ign.flags_mut().insert(SigFlags::SA_RESTART);

    r[Signal::SIGURG as usize - 1] = sig_ign;
    r[Signal::SIGWINCH as usize - 1] = sig_ign;

    r
}

impl ProcessControlBlock {
    /// 刷新指定进程的sighand的sigaction，将满足条件的sigaction恢复为默认状态。
    /// 除非某个信号被设置为忽略且 `force_default` 为 `false`，否则都不会将其恢复。
    ///
    /// # 参数
    ///
    /// - `pcb`: 要被刷新的pcb。
    /// - `force_default`: 是否强制将sigaction恢复成默认状态。
    pub fn flush_signal_handlers(&self, force_default: bool) {
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        // debug!("hand=0x{:018x}", hand as *const sighand_struct as usize);
        let sighand = self.sighand();
        let actions = &mut sighand.inner_mut().handlers;

        for sigaction in actions.iter_mut() {
            if force_default || !sigaction.is_ignore() {
                sigaction.set_action(SigactionType::SaHandler(SaHandlerType::Default));
            }
            // 清除flags中，除了DFL和IGN以外的所有标志
            sigaction.set_restorer(None);
            *sigaction.mask_mut() = SigSet::empty();
            *sigaction.flags_mut() = SigFlags::empty();
            compiler_fence(core::sync::atomic::Ordering::SeqCst);
        }
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }
}

pub(super) fn do_sigaction(
    sig: Signal,
    act: Option<&mut Sigaction>,
    old_act: Option<&mut Sigaction>,
) -> Result<(), SystemError> {
    if sig == Signal::INVALID {
        return Err(SystemError::EINVAL);
    }

    let pcb = ProcessManager::current_pcb();
    let sighand = pcb.sighand();
    let mut sighand_guard = sighand.inner_mut();
    // 指向当前信号的action的引用
    let action: &mut Sigaction = &mut sighand_guard.handlers[SigHand::sig2idx(sig)];

    // 对比 MUSL 和 relibc ， 暂时不设置这个标志位
    // if action.flags().contains(SigFlags::SA_FLAG_IMMUTABLE) {
    //     return Err(SystemError::EINVAL);
    // }

    // 保存原有的 sigaction
    let mut old_act: Option<&mut Sigaction> = {
        if let Some(oa) = old_act {
            *(oa) = *action;
            Some(oa)
        } else {
            None
        }
    };
    // 清除所有的脏的sa_flags位（也就是清除那些未使用的）
    let mut act = {
        if let Some(ac) = act {
            *ac.flags_mut() &= SigFlags::SA_ALL;
            Some(ac)
        } else {
            None
        }
    };

    if let Some(act) = &mut old_act {
        *act.flags_mut() &= SigFlags::SA_ALL;
    }

    if let Some(ac) = &mut act {
        // 将act.sa_mask的SIGKILL SIGSTOP的屏蔽清除
        ac.mask_mut()
            .remove(<Signal as Into<SigSet>>::into(Signal::SIGKILL) | Signal::SIGSTOP.into());

        // 将新的sigaction拷贝到进程的action中
        *action = **ac;
        /*
        * 根据POSIX 3.3.1.3规定：
        * 1.不管一个信号是否被阻塞，只要将其设置SIG_IGN，如果当前已经存在了正在pending的信号，那么就把这个信号忽略。
        *
        * 2.不管一个信号是否被阻塞，只要将其设置SIG_DFL，如果当前已经存在了正在pending的信号，
              并且对这个信号的默认处理方式是忽略它，那么就会把pending的信号忽略。
        */
        if action.is_ignore() {
            let mut mask: SigSet = SigSet::from_bits_truncate(0);
            mask.insert(sig.into());
            pcb.sig_info_mut().sig_pending_mut().flush_by_mask(&mask);
            // todo: 当有了多个线程后，在这里进行操作，把每个线程的sigqueue都进行刷新
        }
    }

    return Ok(());
}
