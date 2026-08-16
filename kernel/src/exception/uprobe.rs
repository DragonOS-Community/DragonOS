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
use crate::libs::rwlock::RwLock;
use crate::mm::ucontext::UprobeInstance;
use crate::mm::MemoryManagementArch;
use crate::mm::VirtAddr;
use crate::process::{ProcessFlags, ProcessManager};
use kprobe::ProbeArgs;
use log::{debug, warn};
use system_error::SystemError;
use uprobe::UprobeOps;

/// `SIGTRAP` 的 `si_code`：breakpoint trap（镜像 Linux `TRAP_BRKPT`，用于未消费的
/// 用户态 #BP）。
const TRAP_BRKPT: i32 = 1;

/// RFLAGS 的 TF（Trap Flag）位——置位后每条指令执行完触发 #DB，用于 XOL 单步。
const RFLAGS_TF: u64 = 1 << 8;

/// Per-thread 活跃 XOL 单步状态（评审 R2/R3/R5/R12）。
#[derive(Debug)]
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
    /// XOL slot 页基址（判别 #DB 是否确来自 slot 内执行；否则走 abort）。
    pub xol_page_base: usize,
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
            return send_sigtrap_brkpt(break_addr);
        }
    };

    // ── Phase 1：uprobe_list 锁内——仅做查表与 Arc 收集（评审 R12）──
    // 短临界区：收集实例 Arc + 首实例的 slot/返回址，回调在锁外执行，
    // 避免持 per-mm 锁跑 BPF 造成长关中断。
    let (insts, xol_slot_offset, return_addr) = {
        let list = mm.uprobe_list.lock_irqsave();
        let Some(entries) = list.get(&break_addr) else {
            drop(list);
            // 无匹配 uprobe → 未消费用户态 #BP → SIGTRAP(TRAP_BRKPT)
            return send_sigtrap_brkpt(break_addr);
        };
        if entries.is_empty() {
            drop(list);
            return send_sigtrap_brkpt(break_addr);
        }
        let insts: Vec<Arc<RwLock<UprobeInstance>>> = entries.clone();
        // XOL 只需执行一次原指令（同址各实例指令相同）；取首个实例的 slot。
        // slot 内容已在注册时预填（uprobe_register 调 build_xol_slot），命中时直接用。
        let pp = insts[0]
            .read()
            .basic
            .probe_point()
            .ok_or(SystemError::EINVAL)?
            .clone();
        (insts, pp.xol_slot_offset, pp.return_address())
    }; // uprobe_list 释放

    // ── Phase 1.5：锁外跑 pre_handler + event_callback（评审 R12）──
    // rip 保持 raw（probe_vaddr+1）：BPF 回调经 break_address()=rip-1 取得原探针址。
    for inst in &insts {
        let g = inst.read();
        if g.basic.is_enabled() {
            g.basic.call_pre_handler(frame);
            g.basic.call_event_callback(frame);
        }
    }

    // ── Phase 2：xol_area 锁内——取 slot 用户址（slot 已预填，无需再写）──
    let (slot_vaddr, xol_page_base) = {
        let guard = mm.xol_area.lock_irqsave();
        let area = guard.as_ref().ok_or(SystemError::EFAULT)?;
        (area.slot_vaddr(xol_slot_offset), area.page_base().data())
    };

    // ── Phase 3：保存 per-thread 活跃状态（评审 R2/R5）→ 重定向 rip → 置 TF ──
    let orig_tf = frame.rflags & RFLAGS_TF != 0;
    {
        let mut ss = pcb.uprobe_ss.lock_irqsave();
        *ss = Some(ActiveXol {
            probe_vaddr: break_addr,
            return_addr,
            orig_tf,
            xol_page_base,
        });
    }
    frame.set_rip(slot_vaddr.data());
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
/// 返回值：`true` = 已消费（XOL 完成或 abort）；`false` = 非 uprobe #DB。
pub fn uprobe_debug_handler(frame: &mut TrapFrame) -> Result<bool, SystemError> {
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

    // ── R3：判别 #DB 是否确来自 XOL slot 内执行 ──
    // 若信号/缺页在本窗口内改道执行（信号 handler 在别处运行后返回），
    // rip 可能不在 XOL 页内——此时按 abort 处理：回到 probe_vaddr 重新执行
    // 原指令（若断点仍在则再次命中 XOL；若已注销则字节已恢复、直接执行）。
    let rip = frame.rip as usize;
    if rip < state.xol_page_base || rip >= state.xol_page_base + crate::arch::MMArch::PAGE_SIZE {
        warn!(
            "uprobe #DB abort: rip {:#x} outside XOL page {:#x}, re-execute probe {:#x}",
            rip, state.xol_page_base, state.probe_vaddr
        );
        frame.set_rip(state.probe_vaddr);
        // 恢复原始 TF（评审 R5）。
        if state.orig_tf {
            frame.rflags |= RFLAGS_TF;
        } else {
            frame.rflags &= !RFLAGS_TF;
        }
        return Ok(true);
    }

    // ── XOL 完成：恢复 rip 到返回址 + 恢复原始 TF（评审 R5）──
    frame.set_rip(state.return_addr);
    if state.orig_tf {
        // 程序/调试器原本就开着单步：保留 TF，使其继续收到预期的单步 #DB
        //（下一个 #DB 将无 NEED_UPROBE，正常走 debug 路径）。
        frame.rflags |= RFLAGS_TF;
    } else {
        frame.rflags &= !RFLAGS_TF;
    }

    // ── post_handler：短锁收集 Arc，锁外执行（评审 R12）──
    if let Some(mm) = pcb.basic().user_vm() {
        let insts: Vec<Arc<RwLock<UprobeInstance>>> = {
            let list = mm.uprobe_list.lock_irqsave();
            list.get(&state.probe_vaddr).cloned().unwrap_or_default()
        };
        for inst in &insts {
            let g = inst.read();
            if g.basic.is_enabled() {
                g.basic.call_post_handler(frame);
            }
        }
    }

    debug!(
        "uprobe XOL single-step done: resume {:#x} (orig_tf={})",
        state.return_addr, state.orig_tf
    );

    Ok(true)
}

/// 向当前进程投递 `SIGTRAP(TRAP_BRKPT)`（未消费的用户态 #BP）。
///
/// 参照 `do_undefined_opcode` 发 `SIGILL` 的模式：先开中断再投递信号。
/// `si_addr` 取断点地址。
fn send_sigtrap_brkpt(break_addr: usize) -> Result<(), SystemError> {
    // #Safety: `interrupt_enable` 仅置位 RFLAGS.IF；此处已脱离 uprobe 命中路径的
    // irqsave 临界区（uprobe_list/xol_area 均已释放），与 do_undefined_opcode 一致。
    unsafe { CurrentIrqArch::interrupt_enable() };
    if let Err(err) =
        force_sig_fault_to_current(Signal::SIGTRAP, TRAP_BRKPT, VirtAddr::new(break_addr))
    {
        warn!(
            "failed to send SIGTRAP(TRAP_BRKPT) for user #BP, pid: {:?}, addr: {:#x}, err: {:?}",
            ProcessManager::current_pid(),
            break_addr,
            err
        );
    }
    Ok(())
}
