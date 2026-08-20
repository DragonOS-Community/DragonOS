//! uprobe 用户态异常分发（计划步骤 5/6/8）。
//!
//! `do_int3`/`do_debug` 按 [`TrapFrame::is_from_user`] 二分：用户态走本模块，
//! 内核态走现有 kprobe 分发（[`crate::exception::ebreak::EBreak`] /
//! [`crate::exception::debug::DebugException`]）。
//!
//! # 命中路径约束（关中断，entry.S `cli`）
//!
//! - 仅 `lock_irqsave` + 查表 + 改 trapframe + 跑 BPF（callback）；
//! - 绝不改页表、绝不睡眠、绝不取 `AddressSpace` 的 `RwSem`（会睡眠）；
//! - XOL slot 内容通过物理页的内核 direct-map 写入（`phys_2_virt`），无需
//!   `PageMapper`/`RwSem`——这是 batch2 `XolArea::page_paddr` 的设计意图。
//!
//! # F4：NEED_UPROBE 是 #DB 分发判别位
//!
//! #BP handler 在重定向 rip 到 XOL slot 前置 `NEED_UPROBE`；用户态 #DB handler
//! 检查并清之以识别「XOL 单步完成的 #DB」，区别于 ptrace/硬件断点 #DB。
//!
//! # F5：BPF 透明性
//!
//! `call_pre_handler`/`call_event_callback`/`call_post_handler` 入口 rip 必须是
//! 原探针语境（`probe_vaddr` 或 `probe_vaddr + insn_len`），XOL slot 用户地址
//! **绝不**暴露给 BPF。

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::interrupt::TrapFrame;
use crate::arch::ipc::signal::Signal;
use crate::arch::CurrentIrqArch;
use crate::exception::InterruptArch;
use crate::ipc::signal::force_sig_fault_to_current;
use crate::mm::ucontext::{UprobeConsumerRuntimeSnapshot, XolSlotLease};
use crate::mm::VirtAddr;
use crate::process::{ProcessControlBlock, ProcessFlags, ProcessManager};
use kprobe::ProbeArgs;
use log::{debug, warn};
use system_error::SystemError;
use uprobe::UprobeOps;

/// `SIGTRAP` 的 `si_code`：breakpoint trap（镜像 Linux `TRAP_BRKPT`，用于未消费的
/// 用户态 #BP）。
const TRAP_BRKPT: i32 = 1;

/// `SIGTRAP` 的 `si_code`：single-step trap。
///
/// Linux 在用户态 TF 单步完成后使用该 code。DragonOS 已在 #DB 入口区分
/// DR6.BS 与 B0-B3，但尚未实现完整 ptrace virtual_dr6。
const TRAP_TRACE: i32 = 2;

/// `SIGTRAP` 的 `si_code`：硬件断点/观察点。
const TRAP_HWBKPT: i32 = 4;

/// x86 DR6 cause bits。
pub const DR6_TRAP_BITS: u64 = 0xf;
pub const DR6_SINGLE_STEP: u64 = 1 << 14;

/// RFLAGS 的 TF（Trap Flag）位——置位后每条指令执行完触发 #DB，用于 XOL 单步。
const RFLAGS_TF: u64 = 1 << 8;

/// XOL 单步的生命周期状态，镜像 Linux 的 `UTASK_SSTEP` 与
/// `UTASK_SSTEP_TRAPPED`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveXolState {
    Running,
    Trapped,
}

/// Per-thread 活跃 XOL 单步状态（评审 R2/R3/R5/R12）。
/// 在 #BP 重定向 rip 到 XOL slot **之前**保存到执行线程的 PCB；
/// #DB 到达时取回——不依赖 uprobe_list/slot 反查，使「另一线程在 XOL
/// 窗口内注销探针并释放 slot」的竞态不影响本线程的恢复语义。
pub struct ActiveXol {
    /// 被探测的原地址（abort 路径重新执行处）。
    pub probe_vaddr: usize,
    /// 原指令执行完毕后的返回地址（= probe_vaddr + insn_len）。
    pub return_addr: usize,
    /// 进入 XOL 前 RFLAGS.TF 的原始值（程序自身/调试器可能已置单步）。
    pub orig_tf: bool,
    /// 原指令在 XOL slot 中执行完毕后的精确 RIP。
    pub slot_end: usize,
    /// Strongly hold the slot lease so concurrent close can only withdraw
    /// future hits and cannot reuse this in-flight instruction slot.
    pub xol_lease: Arc<XolSlotLease>,
    /// #BP 时 enabled consumer 的稳定快照。post handler 必须遍历它，不能从
    /// 可能已被 close 删除的 per-mm 命中表重新查找。
    pub participants: Arc<Vec<UprobeConsumerRuntimeSnapshot>>,
    pub state: ActiveXolState,
}

impl core::fmt::Debug for ActiveXol {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ActiveXol")
            .field("probe_vaddr", &self.probe_vaddr)
            .field("return_addr", &self.return_addr)
            .field("orig_tf", &self.orig_tf)
            .field("slot_end", &self.slot_end)
            .field("xol_slot_offset", &self.xol_lease.offset())
            .field("participant_count", &self.participants.len())
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// 用户态 #BP（int3）分发：uprobe 命中则 XOL 单步原指令，否则投递
/// `SIGTRAP(TRAP_BRKPT)`。
///
/// 调用方（`do_int3`）已保证 `is_from_user()` 为真。
pub fn uprobe_breakpoint_handler(frame: &mut TrapFrame) -> Result<(), SystemError> {
    let break_addr = frame.break_address(); // = rip - 1 = probe_vaddr（raw rip 保留，
                                            // 供 BPF 回调经 break_address() 取得原探针址——batch3↔batch4 运行时契约）。

    // F5：回调执行期间 rip 必须保持 raw（probe_vaddr+1），绝不在此预设为原探针址，
    // 否则回调内 `break_address()=rip-1` 会得到 probe_vaddr-1 而非 probe_vaddr。
    // XOL slot 用户址仅在所有回调返回后才写入 rip，绝不暴露给 BPF。

    let pcb = ProcessManager::current_pcb();
    let mm = match pcb.basic().user_vm() {
        Some(mm) => mm,
        None => {
            // 用户态 #BP 但无 user_vm（不应发生）：防御性投递 SIGTRAP。
            return send_sigtrap_brkpt(frame, break_addr);
        }
    };

    // ── Phase 1：uprobe_list 锁内——仅做查表与 Arc 收集（评审 R12）──
    // 短临界区：收集实例 Arc + 首实例的 slot/返回址，回调在锁外执行，
    // 避免持 per-mm 锁跑 BPF 造成长关中断。
    let (xol_lease, slot_vaddr, slot_end, return_addr, participants) = {
        let list = mm.uprobe_list.lock_irqsave();
        let Some(entries) = list.get(&break_addr) else {
            drop(list);
            // 无匹配 uprobe → 未消费用户态 #BP → SIGTRAP(TRAP_BRKPT)
            return send_sigtrap_brkpt(frame, break_addr);
        };
        if entries.is_empty() {
            drop(list);
            return send_sigtrap_brkpt(frame, break_addr);
        }
        let first = entries[0].read();
        let pp = first.point.clone();
        let site = first.site.clone();
        let xol_lease = first.xol_lease.clone();
        let slot_vaddr = xol_lease.slot_vaddr().data();
        let slot_end = slot_vaddr
            .checked_add(pp.insn_len())
            .ok_or(SystemError::EINVAL)?;
        let return_addr = pp.return_address();
        drop(first);

        let participants = site.participants.read().clone();
        (xol_lease, slot_vaddr, slot_end, return_addr, participants)
    }; // uprobe_list 释放

    // #BP uses a DPL=3 interrupt gate so teardown cannot observe this CPU's
    // shootdown acknowledgement before the hit-table lookup and slot lease
    // capture are complete. The XOL VMA is immutable, so callbacks may run
    // from this point with normal user-exception interrupt semantics.
    unsafe { CurrentIrqArch::interrupt_enable() };

    // ── Phase 1.5：锁外跑 pre_handler + event_callback（评审 R12）──
    // rip 保持 raw（probe_vaddr+1）：BPF 回调经 break_address()=rip-1 取得原探针址。
    for participant in participants.iter() {
        if !participant.permits_task(&pcb) {
            continue;
        }
        (participant.pre_handler)(frame);
        if let Some(callback) = participant.event_callback.as_ref() {
            callback.call(frame);
        }
    }

    // ── Phase 2：保存 per-thread 活跃状态（评审 R2/R5）→ 重定向 rip → 置 TF ──
    let orig_tf = frame.rflags & RFLAGS_TF != 0;
    {
        let mut ss = pcb.uprobe_ss.lock_irqsave();
        *ss = Some(ActiveXol {
            probe_vaddr: break_addr,
            return_addr,
            orig_tf,
            slot_end,
            xol_lease,
            participants,
            state: ActiveXolState::Running,
        });
    }
    frame.set_rip(slot_vaddr);
    frame.rflags |= RFLAGS_TF;
    pcb.flags().insert(ProcessFlags::NEED_UPROBE);

    Ok(())
}

/// 用户态 #DB 分发：`NEED_UPROBE` 置位 → XOL 单步完成（消费）；否则本异常
/// **不属于 uprobe**，返回 `false` 交由调用方路由到正常 debug 路径
/// （ptrace 单步 / 硬件断点 / SIGTRAP，评审 R4）。
///
/// 调用方（`do_debug`）已保证 `is_from_user()` 为真。
///
/// 返回值：`true` = 精确完成本次 XOL；`false` = 非 uprobe #DB，或异常 #DB
/// 已 abort 但仍需走正常用户 SIGTRAP 路径。
pub fn uprobe_debug_handler(frame: &mut TrapFrame, dr6: u64) -> Result<bool, SystemError> {
    let pcb = ProcessManager::current_pcb();

    // ── F4：NEED_UPROBE 是 #DB 判别位 ──
    if !pcb.flags().contains(ProcessFlags::NEED_UPROBE) {
        // 非 uprobe 单步 #DB：不吞掉（评审 R4），交还 do_debug 走正常
        // DebugException 路径（ptrace / 硬件断点 / SIGTRAP）。
        return Ok(false);
    }

    // 清 NEED_UPROBE + 取回 per-thread 活跃状态（评审 R2/R12：O(1)，
    // 不经 uprobe_list/slot 反查——注销竞态下本线程仍能正确恢复）。
    pcb.flags().remove(ProcessFlags::NEED_UPROBE);
    let state = { pcb.uprobe_ss.lock_irqsave().take() };
    let Some(state) = state else {
        // NEED_UPROBE 置位但无活跃状态（不应发生）：防御性按未消费处理。
        warn!(
            "uprobe #DB: NEED_UPROBE set but no active state @ rip {:#x}",
            frame.rip
        );
        return Ok(false);
    };

    // 只有原指令在本租约的 slot 内恰好执行完毕，才是本次 XOL 的完成 #DB。
    // 页范围判断会把硬件断点或异常改道后的 #DB 错认成完成。
    let rip = frame.rip as usize;
    if state.state != ActiveXolState::Running || rip != state.slot_end || dr6 & DR6_SINGLE_STEP == 0
    {
        warn!(
            "uprobe #DB abort: rip {:#x}, expected {:#x}, dr6 {:#x}, state {:?}, re-execute probe {:#x}",
            rip, state.slot_end, dr6, state.state, state.probe_vaddr
        );
        restore_after_abort(frame, &state);
        pcb.recalc_sigpending();
        // 当前 #DB 不是 XOL 完成事件，交给用户 debug/SIGTRAP 路径。
        return Ok(false);
    }

    // ── XOL 完成：恢复 rip 到返回址 + 恢复原始 TF（评审 R5）──
    frame.set_rip(state.return_addr);
    if state.orig_tf {
        // 这次 XOL 就是调试器要求单步的那一条指令。Linux
        // arch_uprobe_post_xol() 在这里立即排队 SIGTRAP；若只保留 TF 等待
        // 下一次 #DB，会额外执行一条真实指令后才通知调试器。
        frame.rflags |= RFLAGS_TF;
    } else {
        frame.rflags &= !RFLAGS_TF;
    }

    // post 使用 #BP 时的 participant 快照。并发 close 可从 mm 表移除实例，
    // 但不能让已经执行过 pre 的 consumer 丢失配对的 post。
    for participant in state.participants.iter() {
        if !participant.permits_task(&pcb) {
            continue;
        }
        (participant.post_handler)(frame);
    }

    debug!(
        "uprobe XOL single-step done: resume {:#x} (orig_tf={})",
        state.return_addr, state.orig_tf
    );

    // 重新发布 XOL 窗口内暂时延迟的普通 pending 信号。
    pcb.recalc_sigpending();

    // 回调与所有 irqsave 临界区结束后再开中断并排队调试信号。
    if state.orig_tf {
        send_sigtrap_trace(state.return_addr)?;
    } else if dr6 & DR6_TRAP_BITS != 0 {
        // uprobe 只消费自己置 TF 产生的 BS；同一 #DB 中并发出现的硬件断点
        // cause 仍必须对用户可见，不能随 XOL 完成一起吞掉。
        send_sigtrap_hwbkpt(state.return_addr)?;
    }

    Ok(true)
}

/// 向当前进程投递 `SIGTRAP(TRAP_BRKPT)`（未消费的用户态 #BP）。
///
/// 参照 `do_undefined_opcode` 发 `SIGILL` 的模式：先开中断再投递信号。
/// `si_addr` 取断点地址。
fn send_sigtrap_brkpt(frame: &mut TrapFrame, break_addr: usize) -> Result<(), SystemError> {
    // 若这个 #BP 来自 XOL 指令本身，它是确定会投递信号的同步异常。
    mark_current_xol_trapped();
    // #Safety: `interrupt_enable` 仅置位 RFLAGS.IF；此处已脱离 uprobe 命中路径的
    // irqsave 临界区（uprobe_list/xol_area 均已释放），与 do_undefined_opcode 一致。
    unsafe { CurrentIrqArch::interrupt_enable() };
    if let Err(err) =
        force_sig_fault_to_current(Signal::SIGTRAP, TRAP_BRKPT, VirtAddr::new(break_addr))
    {
        abort_current_xol(frame);
        warn!(
            "failed to send SIGTRAP(TRAP_BRKPT) for user #BP, pid: {:?}, addr: {:#x}, err: {:?}",
            ProcessManager::current_pid(),
            break_addr,
            err
        );
    }
    Ok(())
}

fn restore_after_abort(frame: &mut TrapFrame, state: &ActiveXol) {
    frame.set_rip(state.probe_vaddr);
    if state.orig_tf {
        frame.rflags |= RFLAGS_TF;
    } else {
        frame.rflags &= !RFLAGS_TF;
    }
}

/// 把当前 XOL 标成已陷阱。幂等；只应在同步异常确定将投递信号后调用。
pub fn mark_current_xol_trapped() -> bool {
    let pcb = ProcessManager::current_pcb();
    let mut active = pcb.uprobe_ss.lock_irqsave();
    let Some(active) = active.as_mut() else {
        return false;
    };
    active.state = ActiveXolState::Trapped;
    true
}

/// 幂等中止当前 XOL，并把 trapframe 恢复为可重试原指令的状态。
pub fn abort_current_xol(frame: &mut TrapFrame) -> bool {
    let pcb = ProcessManager::current_pcb();
    pcb.flags().remove(ProcessFlags::NEED_UPROBE);
    let state = pcb.uprobe_ss.lock_irqsave().take();
    let Some(state) = state else {
        return false;
    };
    restore_after_abort(frame, &state);
    drop(state);
    pcb.recalc_sigpending();
    true
}

/// exec/exit 等不再返回旧用户上下文的路径可调用此函数释放 ActiveXol 的
/// site/slot/consumer 强引用。无需也不应修改一个即将废弃的 trapframe。
pub fn cleanup_task_active_xol(pcb: &ProcessControlBlock) {
    pcb.flags().remove(ProcessFlags::NEED_UPROBE);
    let state = pcb.uprobe_ss.lock_irqsave().take();
    drop(state);
    pcb.recalc_sigpending();
}

/// 信号递送门：返回 `false` 表示普通异步信号需要延迟到本条 XOL 指令完成。
/// fatal 或已标记 Trapped 的路径会先 abort，再允许信号构造用户 frame。
pub fn signal_gate(frame: &mut TrapFrame) -> bool {
    let pcb = ProcessManager::current_pcb();
    let state = pcb
        .uprobe_ss
        .lock_irqsave()
        .as_ref()
        .map(|active| active.state);
    let Some(state) = state else {
        return true;
    };

    // group-exit 与线程/共享 SIGKILL 都必须立刻终止 XOL。该 helper 虽因 OOM
    // 命名，但语义正是这里需要的完整 fatal-pending 查询。
    let fatal = Signal::oom_fatal_signal_pending(&pcb);
    if state == ActiveXolState::Running && !fatal {
        // pending 队列保持不变；只暂时清 fast flag，避免 exit-to-user loop
        // 原地重入。XOL 的紧邻 #DB 完成/abort 会 recalc 并重新置位。
        pcb.flags().remove(ProcessFlags::HAS_PENDING_SIGNAL);
        // 与并发 signal sender 再校验一次：若 fatal 在首次查询和清 flag 之间
        // 到达，必须本次就 abort；若在本次查询之后到达，sender 会重新置
        // HAS_PENDING_SIGNAL，exit-to-user loop 会再次进入本 gate。
        if Signal::oom_fatal_signal_pending(&pcb) {
            mark_current_xol_trapped();
            abort_current_xol(frame);
            return true;
        }
        return false;
    }

    if fatal {
        mark_current_xol_trapped();
    }
    abort_current_xol(frame);
    true
}

/// 投递用户态单步产生的 `SIGTRAP(TRAP_TRACE)`。
///
/// 该入口只负责用户调试异常，不得路由到仅处理内核 kprobe 的
/// `DebugException`。
pub fn send_sigtrap_trace(addr: usize) -> Result<(), SystemError> {
    unsafe { CurrentIrqArch::interrupt_enable() };
    if let Err(err) = force_sig_fault_to_current(Signal::SIGTRAP, TRAP_TRACE, VirtAddr::new(addr)) {
        warn!(
            "failed to send SIGTRAP(TRAP_TRACE), pid: {:?}, addr: {:#x}, err: {:?}",
            ProcessManager::current_pid(),
            addr,
            err
        );
    }
    Ok(())
}

fn send_sigtrap_hwbkpt(addr: usize) -> Result<(), SystemError> {
    send_sigtrap_fault(TRAP_HWBKPT, addr, "TRAP_HWBKPT")
}

/// 投递未被 uprobe 消费的用户 #DB。DragonOS 尚无完整 ptrace virtual_dr6，
/// 但至少按 Linux get_si_code() 的优先级保留 single-step 与 hardware cause。
pub fn send_user_debug_sigtrap(addr: usize, dr6: u64) -> Result<(), SystemError> {
    if dr6 & DR6_SINGLE_STEP != 0 {
        send_sigtrap_trace(addr)
    } else if dr6 & DR6_TRAP_BITS != 0 {
        send_sigtrap_hwbkpt(addr)
    } else {
        send_sigtrap_fault(TRAP_BRKPT, addr, "TRAP_BRKPT")
    }
}

fn send_sigtrap_fault(code: i32, addr: usize, name: &str) -> Result<(), SystemError> {
    unsafe { CurrentIrqArch::interrupt_enable() };
    if let Err(err) = force_sig_fault_to_current(Signal::SIGTRAP, code, VirtAddr::new(addr)) {
        warn!(
            "failed to send SIGTRAP({}), pid: {:?}, addr: {:#x}, err: {:?}",
            name,
            ProcessManager::current_pid(),
            addr,
            err
        );
    }
    Ok(())
}
