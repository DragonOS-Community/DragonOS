use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::Ordering;
use system_error::SystemError;

use crate::{
    arch::ipc::signal::{SigChildCode, Signal},
    driver::tty::tty_core::TtyCore,
    ipc::signal_types::SignalFlags,
    process::{pid::PidType, ptrace, wait::WaitSelector},
    syscall::user_access::UserBufferWriter,
};

use super::{
    abi::WaitOption,
    dec_visible_thread_count,
    resource::{RUsage, RUsageWho},
    ProcessControlBlock, ProcessFlags, ProcessManager, RawPid,
};

/// 将内核中保存的 wstatus（已经按 wait4 语义左移过的编码值）
/// 转换为 waitid 语义下的 si_status（低 8 位退出码）。
#[inline(always)]
fn wstatus_to_waitid_status(raw_wstatus: i32) -> i32 {
    (raw_wstatus >> 8) & 0xff
}

#[inline(always)]
fn wstatus_to_waitid_exit_info(raw_wstatus: i32) -> (i32, i32) {
    let signal = raw_wstatus & 0x7f;
    if signal == 0 {
        (
            wstatus_to_waitid_status(raw_wstatus),
            SigChildCode::Exited.into(),
        )
    } else if (raw_wstatus & 0x80) != 0 {
        (signal, SigChildCode::Dumped.into())
    } else {
        (signal, SigChildCode::Killed.into())
    }
}

/// mt-exec: de_thread 正在等待旧 leader 完成 PID/TID 交换时，禁止提前回收
fn reap_blocked_by_group_exec(child_pcb: &Arc<ProcessControlBlock>) -> bool {
    if !child_pcb.is_thread_group_leader() {
        return false;
    }
    if !child_pcb.sighand().flags_contains(SignalFlags::GROUP_EXEC) {
        return false;
    }
    let exec_task = child_pcb.sighand().group_exec_task();
    exec_task
        .as_ref()
        .map(|t| !Arc::ptr_eq(t, child_pcb))
        .unwrap_or(true)
}

fn delay_group_leader(child_pcb: &Arc<ProcessControlBlock>) -> bool {
    if !child_pcb.is_thread_group_leader() {
        return false;
    }

    child_pcb
        .threads_read_irqsave()
        .group_tasks_clone()
        .iter()
        .filter_map(Weak::upgrade)
        .any(|task| !task.is_exited() && !task.is_zombie() && !task.is_dead())
}

/// mt-exec: 非执行线程的组长在退出时，延迟 PID/TGID/PGID/SID 的 unhash
/// 以避免 de_thread 交换 TID/raw_pid 时出现 ESRCH。
fn should_defer_unhash_for_group_exec(pcb: &ProcessControlBlock, group_dead: bool) -> bool {
    if !group_dead {
        return false;
    }
    let sighand = pcb.sighand();
    if !sighand.flags_contains(SignalFlags::GROUP_EXEC) {
        return false;
    }
    let exec_task = sighand.group_exec_task();
    let this = pcb.self_ref.upgrade();
    match (exec_task, this) {
        (Some(exec_task), Some(this)) => !Arc::ptr_eq(&exec_task, &this),
        _ => false,
    }
}

/// 检查子进程的 exit_signal 是否与等待选项匹配
///
/// 根据 Linux wait 语义：
/// - __WALL: 等待所有子进程，忽略 exit_signal
/// - __WCLONE: 只等待"克隆"子进程（exit_signal != SIGCHLD）
/// - 默认（无 __WCLONE）: 只等待"正常"子进程（exit_signal == SIGCHLD）
fn child_matches_wait_options(
    child_pcb: &Arc<ProcessControlBlock>,
    options: WaitOption,
    relation: WaitRelation,
) -> bool {
    if relation == WaitRelation::Ptraced {
        return true;
    }

    // __WALL 匹配所有子进程
    if options.contains(WaitOption::WALL) {
        return true;
    }

    let child_exit_signal = child_pcb.exit_signal.load(Ordering::SeqCst);
    let is_clone_child = child_exit_signal != Signal::SIGCHLD as i32;
    let wants_clone = options.contains(WaitOption::WCLONE);

    // 子进程类型必须与等待选项匹配
    is_clone_child == wants_clone
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WaitRelation {
    Natural,
    Ptraced,
}

#[derive(Clone, Copy)]
struct WaitRelations(u8);

impl WaitRelations {
    const NATURAL: u8 = 1 << 0;
    const PTRACED: u8 = 1 << 1;

    fn empty() -> Self {
        Self(0)
    }

    fn insert(&mut self, relation: WaitRelation) {
        self.0 |= match relation {
            WaitRelation::Natural => Self::NATURAL,
            WaitRelation::Ptraced => Self::PTRACED,
        };
    }

    fn contains(self, relation: WaitRelation) -> bool {
        let bit = match relation {
            WaitRelation::Natural => Self::NATURAL,
            WaitRelation::Ptraced => Self::PTRACED,
        };
        self.0 & bit != 0
    }
}

struct WaitCandidate {
    child: Arc<ProcessControlBlock>,
    relations: WaitRelations,
}

fn push_wait_candidate(
    candidates: &mut Vec<WaitCandidate>,
    child: Arc<ProcessControlBlock>,
    relation: WaitRelation,
) {
    let raw_pid = child.raw_pid();
    if let Some(candidate) = candidates.iter_mut().find(|p| p.child.raw_pid() == raw_pid) {
        candidate.relations.insert(relation);
        return;
    }

    let mut relations = WaitRelations::empty();
    relations.insert(relation);
    candidates.push(WaitCandidate { child, relations });
}

fn wait_candidate_children(options: WaitOption) -> Vec<WaitCandidate> {
    let current = ProcessManager::current_pcb();
    let leader = get_thread_group_leader(&current);

    let natural_owners = if options.contains(WaitOption::WNOTHREAD) {
        // DragonOS stores natural children on the thread-group leader. Linux's
        // __WNOTHREAD eligibility is still enforced later through fork_parent.
        vec![leader.clone()]
    } else {
        ProcessManager::thread_group_tasks_snapshot(leader.clone())
    };

    let mut candidates = Vec::new();
    for waiter in natural_owners {
        let parent_ns = waiter.active_pid_ns();
        for pid in waiter.children.read().iter().copied() {
            if let Some(pcb) = ProcessManager::find_task_by_pid_ns(pid, &parent_ns) {
                push_wait_candidate(&mut candidates, pcb, WaitRelation::Natural);
            }
        }
    }

    let ptrace_waiters = if options.contains(WaitOption::WNOTHREAD) {
        vec![current]
    } else {
        ProcessManager::thread_group_tasks_snapshot(leader)
    };
    for waiter in ptrace_waiters {
        for pid in ptrace::tracees_of(&waiter) {
            if let Some(pcb) = ProcessManager::find(pid) {
                push_wait_candidate(&mut candidates, pcb, WaitRelation::Ptraced);
            }
        }
    }
    candidates
}

fn fill_wait_rusage(child_pcb: &Arc<ProcessControlBlock>, kwo: &mut KernelWaitOption) -> RUsage {
    let usage = child_pcb
        .get_rusage(RUsageWho::RUsageBoth)
        .unwrap_or_default();
    if let Some(ret_rusage) = kwo.ret_rusage.as_deref_mut() {
        *ret_rusage = usage;
    }
    usage
}

fn account_reaped_child_rusage(child_rusage: &RUsage) {
    ProcessManager::current_pcb().add_child_rusage(child_rusage);
}

/// 内核wait4时的参数
#[derive(Debug)]
pub struct KernelWaitOption<'a> {
    pub selector: WaitSelector,
    pub options: WaitOption,
    pub ret_status: i32,
    pub ret_info: Option<WaitIdInfo>,
    pub ret_rusage: Option<&'a mut RUsage>,
    pub no_task_error: Option<SystemError>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct WaitIdInfo {
    pub pid: RawPid,
    pub status: i32,
    pub cause: i32,
}

impl KernelWaitOption<'_> {
    pub fn new(selector: WaitSelector, options: WaitOption) -> Self {
        Self {
            selector,
            options,
            ret_status: 0,
            ret_info: None,
            ret_rusage: None,
            no_task_error: None,
        }
    }
}

pub fn kernel_wait4(
    pid: i32,
    options: WaitOption,
    rusage_buf: Option<&mut RUsage>,
) -> Result<(usize, i32), SystemError> {
    let selector = WaitSelector::from_wait4_pid(pid)?;

    // 构造参数
    let mut kwo = KernelWaitOption::new(selector, options);

    kwo.options.insert(WaitOption::WEXITED);
    kwo.ret_rusage = rusage_buf;

    // 调用do_wait，执行等待
    let r = do_wait(&mut kwo)?;

    Ok((r, kwo.ret_status))
}

/// waitid 的内核实现：基于 do_wait，返回 0，必要时写回 siginfo 与 rusage
pub fn kernel_waitid(
    pid_selector: WaitSelector,
    mut infop: Option<UserBufferWriter<'_>>, // PosixSigInfo
    options: WaitOption,
    rusage_buf: Option<&mut RUsage>,
) -> Result<bool, SystemError> {
    // 构造参数
    let mut kwo = KernelWaitOption::new(pid_selector, options);
    kwo.ret_rusage = rusage_buf;
    // waitid 不强制 WEXITED，由调用者通过 options 指定

    // 走通用等待
    let _ = do_wait(&mut kwo)?;

    // 写回 siginfo（若提供）
    if let Some(mut writer) = infop.take() {
        // log::debug!(
        //     "kernel_waitid: about to write PosixSigInfo, sizeof={} bytes, user_buf_size={} bytes",
        //     core::mem::size_of::<PosixSigInfo>(),
        //     writer.size()
        // );
        use crate::ipc::signal_types::{PosixSigInfo, PosixSiginfoFields, PosixSiginfoSigchld};
        let mut si = PosixSigInfo {
            si_signo: 0,
            si_errno: 0,
            si_code: 0,
            _sifields: PosixSiginfoFields {
                _kill: crate::ipc::signal_types::PosixSiginfoKill {
                    si_pid: 0,
                    si_uid: 0,
                },
            },
        };
        if let Some(info) = &kwo.ret_info {
            si.si_signo = Signal::SIGCHLD as i32; // SIGCHLD
            si.si_errno = 0;
            si.si_code = info.cause; // CLD_*
            si._sifields = PosixSiginfoFields {
                _sigchld: PosixSiginfoSigchld {
                    si_pid: info.pid.data() as i32,
                    si_uid: 0,
                    si_status: info.status,
                    si_utime: 0,
                    si_stime: 0,
                },
            };
        }
        writer.copy_one_to_user(&si, 0)?;
        // if let Some(info) = &kwo.ret_info {
        //     log::debug!(
        //         "kernel_waitid: wrote siginfo: signo={}, code={}, pid={}, status={}",
        //         si.si_signo,
        //         si.si_code,
        //         info.pid.data(),
        //         info.status
        //     );
        // } else {
        //     log::debug!(
        //         "kernel_waitid: wrote empty siginfo (no event): signo=0, code=0"
        //     );
        // }
    }

    Ok(kwo.ret_info.is_some())
}

/// 检查子进程是否可以被当前线程等待
///
/// 根据 Linux wait 语义：
/// - 默认情况下，线程组中的任何线程都可以等待同一线程组中任何线程 fork 的子进程
/// - 如果指定了 __WNOTHREAD，则只能等待当前线程自己创建的子进程
///
/// # 参数
/// - `child_pcb`: 要检查的子进程
/// - `options`: 等待选项
///
/// # 返回值
/// 返回 true 如果当前线程可以等待该子进程
fn is_eligible_child(child_pcb: &Arc<ProcessControlBlock>, options: WaitOption) -> bool {
    let current = ProcessManager::current_pcb();
    let current_tgid = current.tgid;

    if options.contains(WaitOption::WNOTHREAD) {
        // 带 __WNOTHREAD：只能等待当前线程自己创建的子进程
        let fork_parent = match child_pcb.fork_parent_pcb() {
            Some(p) => p,
            None => return false,
        };
        Arc::ptr_eq(&fork_parent, &current)
    } else {
        // 获取子进程的 real_parent
        let child_parent = match child_pcb.real_parent_pcb() {
            Some(p) => p,
            None => {
                // log::warn!(
                //     "is_eligible_child: child {:?} has no real parent",
                //     child_pcb.raw_pid()
                // );
                return false;
            }
        };
        // 默认情况：线程组中的任何线程都可以等待同一线程组中任何线程创建的子进程
        // 检查子进程的 real_parent 的 tgid 是否与当前线程的 tgid 相同
        let res = child_parent.tgid == current_tgid;
        if !res {
            // log::warn!(
            //     "is_eligible_child failed: child={:?} child_parent={:?} (tgid={:?}) current={:?} (tgid={:?})",
            //     child_pcb.raw_pid(),
            //     child_parent.raw_pid(),
            //     child_parent.tgid,
            //     current.raw_pid(),
            //     current_tgid
            // );
        }
        res
    }
}

/// 获取当前线程组 leader 的 PCB
///
/// 用于在 wait 时遍历整个线程组的 children
fn get_thread_group_leader(pcb: &Arc<ProcessControlBlock>) -> Arc<ProcessControlBlock> {
    let ti = pcb.thread.read_irqsave();
    ti.group_leader().unwrap_or_else(|| pcb.clone())
}

fn wait_visible_pid(child_pcb: &Arc<ProcessControlBlock>) -> RawPid {
    let current = ProcessManager::current_pcb();
    let leader = get_thread_group_leader(&current);
    child_pcb
        .task_pid_nr_ns(PidType::PID, Some(leader.active_pid_ns()))
        .unwrap_or(RawPid(0))
}

enum CandidateDecision {
    Ready(Result<usize, SystemError>),
    Pending { can_change: bool },
    Ineligible,
}

struct ScanDecision {
    ready: Option<Result<usize, SystemError>>,
    has_eligible: bool,
    has_future_event: bool,
}

impl ScanDecision {
    fn new() -> Self {
        Self {
            ready: None,
            has_eligible: false,
            has_future_event: false,
        }
    }

    fn observe(&mut self, decision: CandidateDecision) {
        match decision {
            CandidateDecision::Ready(result) => {
                self.ready = Some(result);
            }
            CandidateDecision::Pending { can_change } => {
                self.has_eligible = true;
                self.has_future_event |= can_change;
            }
            CandidateDecision::Ineligible => {}
        }
    }
}

fn relation_is_eligible(
    child_pcb: &Arc<ProcessControlBlock>,
    relation: WaitRelation,
    options: WaitOption,
) -> bool {
    match relation {
        WaitRelation::Natural => is_eligible_child(child_pcb, options),
        WaitRelation::Ptraced => {
            let current = ProcessManager::current_pcb();
            ptrace::is_wait_tracee_of(child_pcb, &current, options)
        }
    }
}

fn report_wait_event(
    child_pcb: &Arc<ProcessControlBlock>,
    relation: WaitRelation,
    kwo: &mut KernelWaitOption,
) -> CandidateDecision {
    if !relation_is_eligible(child_pcb, relation, kwo.options)
        || !child_matches_wait_options(child_pcb, kwo.options, relation)
    {
        return CandidateDecision::Ineligible;
    }

    let state = child_pcb.sched_info().state();

    // Linux wait_consider_task() checks zombie before stopped/continued.
    // A zombie leader with live subthreads is still an eligible child even when
    // the caller did not request WEXITED; otherwise waitid(WSTOPPED|WNOHANG)
    // would incorrectly report ECHILD while the thread group can still change.
    let delayed_zombie = child_pcb.is_zombie()
        && (delay_group_leader(child_pcb) || reap_blocked_by_group_exec(child_pcb));
    if child_pcb.is_zombie() && !delayed_zombie && kwo.options.contains(WaitOption::WEXITED) {
        let Some(raw_wstatus) = state.raw_wstatus().map(|status| status as i32) else {
            return CandidateDecision::Pending { can_change: false };
        };
        if !kwo.options.contains(WaitOption::WNOWAIT) && !child_pcb.try_mark_dead_from_zombie() {
            return CandidateDecision::Ineligible;
        }

        let pid = wait_visible_pid(child_pcb);
        let (status, cause) = wstatus_to_waitid_exit_info(raw_wstatus);
        let child_rusage = fill_wait_rusage(child_pcb, kwo);
        kwo.no_task_error = None;
        kwo.ret_status = raw_wstatus;
        kwo.ret_info = Some(WaitIdInfo { pid, status, cause });

        if !kwo.options.contains(WaitOption::WNOWAIT) {
            account_reaped_child_rusage(&child_rusage);
            unsafe { ProcessManager::release(child_pcb.raw_pid()) };
        }

        return CandidateDecision::Ready(Ok(pid.into()));
    }

    let stop_requested =
        relation == WaitRelation::Ptraced || kwo.options.contains(WaitOption::WSTOPPED);
    if state.is_stopped()
        && stop_requested
        && child_pcb.sighand().flags_test_and_clear(
            SignalFlags::CLD_STOPPED,
            !kwo.options.contains(WaitOption::WNOWAIT),
        )
    {
        let stopsig = child_pcb.sighand().stop_signal() as i32;
        let cause = if relation == WaitRelation::Ptraced {
            SigChildCode::Trapped.into()
        } else {
            SigChildCode::Stopped.into()
        };
        kwo.no_task_error = None;
        kwo.ret_info = Some(WaitIdInfo {
            pid: wait_visible_pid(child_pcb),
            status: stopsig,
            cause,
        });
        kwo.ret_status = (stopsig << 8) | 0x7f;
        fill_wait_rusage(child_pcb, kwo);
        return CandidateDecision::Ready(Ok(wait_visible_pid(child_pcb).into()));
    }

    if kwo.options.contains(WaitOption::WCONTINUED)
        && child_pcb.sighand().flags_test_and_clear(
            SignalFlags::CLD_CONTINUED,
            !kwo.options.contains(WaitOption::WNOWAIT),
        )
    {
        kwo.no_task_error = None;
        kwo.ret_info = Some(WaitIdInfo {
            pid: wait_visible_pid(child_pcb),
            status: Signal::SIGCONT as i32,
            cause: SigChildCode::Continued.into(),
        });
        kwo.ret_status = 0xffff;
        fill_wait_rusage(child_pcb, kwo);
        return CandidateDecision::Ready(Ok(wait_visible_pid(child_pcb).into()));
    }

    let can_change = if child_pcb.is_zombie() {
        delayed_zombie
            && (relation == WaitRelation::Natural
                || kwo.options.contains(WaitOption::WEXITED)
                || kwo.options.contains(WaitOption::WCONTINUED))
    } else {
        true
    };
    CandidateDecision::Pending { can_change }
}

fn report_candidate_relation(
    child_pcb: &Arc<ProcessControlBlock>,
    relation: WaitRelation,
    kwo: &mut KernelWaitOption,
    scan: &mut ScanDecision,
) -> bool {
    let decision = report_wait_event(child_pcb, relation, kwo);
    let ready = matches!(decision, CandidateDecision::Ready(_));
    scan.observe(decision);
    ready
}

fn report_candidate(
    candidate: &WaitCandidate,
    kwo: &mut KernelWaitOption,
    scan: &mut ScanDecision,
) -> bool {
    // A tracee that is also a natural child should be observed through the
    // ptrace relation first, matching Linux's wait_consider_task() switch to
    // ptrace semantics for children traced by the caller's thread group.
    if candidate.relations.contains(WaitRelation::Ptraced)
        && report_candidate_relation(&candidate.child, WaitRelation::Ptraced, kwo, scan)
    {
        return true;
    }
    if candidate.relations.contains(WaitRelation::Natural)
        && report_candidate_relation(&candidate.child, WaitRelation::Natural, kwo, scan)
    {
        return true;
    }
    false
}

fn scan_wait_candidates<F>(
    kwo: &mut KernelWaitOption,
    candidates: &[WaitCandidate],
    mut matches_selector: F,
) -> ScanDecision
where
    F: FnMut(&Arc<ProcessControlBlock>) -> bool,
{
    let mut scan = ScanDecision::new();
    for candidate in candidates {
        if !matches_selector(&candidate.child) {
            continue;
        }
        if report_candidate(candidate, kwo, &mut scan) {
            break;
        }
    }
    scan
}

fn scan_result_or_wait(scan: ScanDecision) -> Result<Option<usize>, SystemError> {
    if let Some(result) = scan.ready {
        return result.map(Some);
    }
    if !scan.has_eligible || !scan.has_future_event {
        return Err(SystemError::ECHILD);
    }
    Ok(None)
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/exit.c#1573
fn do_wait(kwo: &mut KernelWaitOption) -> Result<usize, SystemError> {
    // todo: 在signal struct里面增加等待队列，并在这里初始化子进程退出的回调，使得子进程退出时，能唤醒当前进程。

    kwo.no_task_error = Some(SystemError::ECHILD);
    let retval = match kwo.selector.clone() {
        WaitSelector::Pid(pid) => {
            if pid.pid_vnr().data() == ProcessManager::current_pcb().raw_tgid().data() {
                return Err(SystemError::ECHILD);
            }

            let current = ProcessManager::current_pcb();
            let wait_queue_owner = get_thread_group_leader(&current);
            let check_child = |kwo: &mut KernelWaitOption| -> Result<Option<usize>, SystemError> {
                let natural_child = pid.thread_group_leader_task();
                let ptrace_child = pid.pid_task(PidType::PID);

                let mut candidates = Vec::new();
                if let Some(child_pcb) = natural_child {
                    push_wait_candidate(&mut candidates, child_pcb, WaitRelation::Natural);
                }
                if let Some(child_pcb) = ptrace_child {
                    if ptrace::is_wait_tracee_of(
                        &child_pcb,
                        &ProcessManager::current_pcb(),
                        kwo.options,
                    ) {
                        push_wait_candidate(&mut candidates, child_pcb, WaitRelation::Ptraced);
                    }
                }

                let mut scan = ScanDecision::new();
                for candidate in &candidates {
                    if report_candidate(candidate, kwo, &mut scan) {
                        break;
                    }
                }

                scan_result_or_wait(scan)
            };

            loop {
                if let Some(pid) = check_child(kwo)? {
                    break Ok(pid);
                }
                if kwo.options.contains(WaitOption::WNOHANG) {
                    break Ok(0);
                }

                let mut ready: Option<Result<Option<usize>, SystemError>> = None;
                let wait_res = wait_queue_owner.wait_queue.wait_event_interruptible(
                    || match check_child(kwo) {
                        Ok(Some(pid)) => {
                            ready = Some(Ok(Some(pid)));
                            true
                        }
                        Ok(None) => false,
                        Err(err) => {
                            ready = Some(Err(err));
                            true
                        }
                    },
                    None::<fn()>,
                );

                match wait_res {
                    Ok(()) => {
                        if let Some(r) = ready.take() {
                            break r.map(|pid| pid.unwrap_or(0));
                        }
                        if ProcessManager::current_pcb().has_pending_signal_fast() {
                            break Err(SystemError::ERESTARTSYS);
                        }
                        // 伪唤醒，继续等待
                        continue;
                    }
                    Err(e) => break Err(e),
                }
            }
        }
        WaitSelector::Any => {
            let current = ProcessManager::current_pcb();
            let wait_queue_owner = get_thread_group_leader(&current);
            loop {
                if kwo.options.contains(WaitOption::WNOHANG) {
                    let candidates = wait_candidate_children(kwo.options);
                    let scan = scan_wait_candidates(kwo, &candidates, |_| true);
                    break scan_result_or_wait(scan).map(|pid| pid.unwrap_or(0));
                }

                let mut ready: Option<Result<Option<usize>, SystemError>> = None;

                let wait_res = wait_queue_owner.wait_queue.wait_event_interruptible(
                    || {
                        let candidates = wait_candidate_children(kwo.options);
                        let scan = scan_wait_candidates(kwo, &candidates, |_| true);
                        match scan_result_or_wait(scan) {
                            Ok(Some(pid)) => {
                                ready = Some(Ok(Some(pid)));
                                true
                            }
                            Ok(None) => false,
                            Err(err) => {
                                ready = Some(Err(err));
                                true
                            }
                        }
                    },
                    None::<fn()>,
                );

                match wait_res {
                    Ok(()) => {
                        if let Some(r) = ready.take() {
                            break r.map(|pid| pid.unwrap_or(0));
                        }
                        if ProcessManager::current_pcb().has_pending_signal_fast() {
                            break Err(SystemError::ERESTARTSYS);
                        }
                        continue;
                    }
                    Err(e) => break Err(e),
                }
            }
        }
        WaitSelector::Pgid(Some(pgid)) => {
            let current = ProcessManager::current_pcb();
            let wait_queue_owner = get_thread_group_leader(&current);
            loop {
                if kwo.options.contains(WaitOption::WNOHANG) {
                    let candidates = wait_candidate_children(kwo.options);
                    let scan = scan_wait_candidates(kwo, &candidates, |pcb| {
                        let child_pgrp = pcb.task_pgrp();
                        match &child_pgrp {
                            Some(cp) => Arc::ptr_eq(cp, &pgid),
                            None => false,
                        }
                    });
                    break scan_result_or_wait(scan).map(|pid| pid.unwrap_or(0));
                }

                let mut ready: Option<Result<Option<usize>, SystemError>> = None;
                let wait_res = wait_queue_owner.wait_queue.wait_event_interruptible(
                    || {
                        let candidates = wait_candidate_children(kwo.options);
                        let scan = scan_wait_candidates(kwo, &candidates, |pcb| {
                            let child_pgrp = pcb.task_pgrp();
                            match &child_pgrp {
                                Some(cp) => Arc::ptr_eq(cp, &pgid),
                                None => false,
                            }
                        });
                        match scan_result_or_wait(scan) {
                            Ok(Some(pid)) => {
                                ready = Some(Ok(Some(pid)));
                                true
                            }
                            Ok(None) => false,
                            Err(err) => {
                                ready = Some(Err(err));
                                true
                            }
                        }
                    },
                    None::<fn()>,
                );

                match wait_res {
                    Ok(()) => {
                        if let Some(r) = ready.take() {
                            break r.map(|pid| pid.unwrap_or(0));
                        }
                        if ProcessManager::current_pcb().has_pending_signal_fast() {
                            break Err(SystemError::ERESTARTSYS);
                        }
                        continue;
                    }
                    Err(e) => break Err(e),
                }
            }
        }

        WaitSelector::Pgid(None) => Err(SystemError::ECHILD),
    };

    return retval;
}

impl ProcessControlBlock {
    fn dec_visible_thread_count_if_accounted(&self) {
        if self.take_visible_thread_accounted() {
            dec_visible_thread_count();
        }
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/exit.c#143
    pub(super) fn __exit_signal(&self) {
        if self.flags().contains(ProcessFlags::PID_UNHASHED) {
            return;
        }
        self.flags().insert(ProcessFlags::PID_UNHASHED);

        let sighand = self.sighand();
        if sighand.flags_contains(SignalFlags::GROUP_EXEC) {
            let this = self.self_ref.upgrade();
            let exec_task = sighand.group_exec_task();
            let should_clear = exec_task
                .as_ref()
                .and_then(|t| this.as_ref().map(|me| Arc::ptr_eq(t, me)))
                .unwrap_or(false);
            if should_clear {
                sighand.finish_group_exec();
            }
        }

        let group_dead = self.is_thread_group_leader();
        let mut sig_guard = self.sig_info_mut();
        let mut tty: Option<Arc<TtyCore>> = None;
        // log::debug!(
        //     "Process {} is exiting, group_dead: {}, state: {:?}",
        //     self.raw_pid(),
        //     group_dead,
        //     self.sched_info().state()
        // );
        if group_dead {
            tty = sig_guard.tty();
            sig_guard.set_tty(None);
        } else {
            // todo: 通知那些等待当前线程组退出的进程
        }
        self.__unhash_process(group_dead);

        drop(tty);
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/exit.c#123
    fn __unhash_process(&self, group_dead: bool) {
        if should_defer_unhash_for_group_exec(self, group_dead) {
            self.flags().insert(ProcessFlags::DEFER_UNHASH);
        } else {
            self.dec_visible_thread_count_if_accounted();
            self.detach_pid(PidType::PID);
            if group_dead {
                self.detach_pid(PidType::TGID);
                self.detach_pid(PidType::PGID);
                self.detach_pid(PidType::SID);
            }
        }

        // 从线程组中移除。非组长线程离开 group_tasks 后，线程组 rusage 仍需保留其 CPU 时间。
        let thread_group_leader = self.threads_read_irqsave().group_leader();
        if let Some(leader) = thread_group_leader {
            let mut leader_threads = leader.threads_write_irqsave();
            if !group_dead {
                if let Some(rusage) = self.get_rusage(RUsageWho::RusageThread) {
                    leader.add_exited_thread_group_rusage(&rusage);
                }
            }
            leader_threads
                .group_tasks
                .retain(|pcb| !Weak::ptr_eq(pcb, &self.self_ref));
        }
    }

    /// 在 de_thread 完成 PID/TID 交换后，补做延迟的 unhash。
    pub(super) fn finish_deferred_unhash_for_exec(&self) {
        if !self.flags().contains(ProcessFlags::DEFER_UNHASH) {
            return;
        }
        self.flags().remove(ProcessFlags::DEFER_UNHASH);
        self.dec_visible_thread_count_if_accounted();
        self.detach_pid(PidType::PID);
        if self.is_thread_group_leader() {
            self.detach_pid(PidType::TGID);
            self.detach_pid(PidType::PGID);
            self.detach_pid(PidType::SID);
        }
    }
}
