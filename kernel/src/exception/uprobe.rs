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

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::interrupt::TrapFrame;
use crate::arch::ipc::signal::Signal;
use crate::arch::CurrentIrqArch;
use crate::exception::InterruptArch;
use crate::ipc::signal::force_sig_fault_to_current;
use crate::libs::rwlock::RwLock;
use crate::mm::ucontext::UprobeInstance;
use crate::mm::VirtAddr;
use crate::process::{ProcessFlags, ProcessManager};
use kprobe::ProbeArgs;
use log::{debug, warn};
use system_error::SystemError;
use uprobe::{UprobeOps, UPROBE_INSN_COPY_SIZE};

/// `SIGTRAP` 的 `si_code`：breakpoint trap（镜像 Linux `TRAP_BRKPT`，用于未消费的
/// 用户态 #BP）。
const TRAP_BRKPT: i32 = 1;

/// RFLAGS 的 TF（Trap Flag）位——置位后每条指令执行完触发 #DB，用于 XOL 单步。
const RFLAGS_TF: u64 = 1 << 8;

/// 用户态 #BP（int3）分发：uprobe 命中则 XOL 单步原指令，否则投递
/// `SIGTRAP(TRAP_BRKPT)`。
///
/// 调用方（`do_int3`）已保证 `is_from_user()` 为真。
pub fn uprobe_breakpoint_handler(frame: &mut TrapFrame) -> Result<(), SystemError> {
    let break_addr = frame.break_address(); // = rip - 1 = probe_vaddr（raw rip 保留，
                                            // 供 BPF 回调经 break_address() 取得原探针址——batch3↔batch4 运行时契约）。

    // F5：回调执行期间 rip 必须保持 raw（probe_vaddr+1），绝不在此预设为原探针址，
    // 否则回调内 `break_address()=rip-1` 会得到 probe_vaddr-1 而非 probe_vaddr。
    // XOL slot 用户址仅在所有回调返回后（Phase 4）才写入 rip，绝不暴露给 BPF。

    let pcb = ProcessManager::current_pcb();
    let mm = match pcb.basic().user_vm() {
        Some(mm) => mm,
        None => {
            // 用户态 #BP 但无 user_vm（不应发生）：防御性投递 SIGTRAP。
            return send_sigtrap_brkpt(break_addr);
        }
    };

    // ── Phase 1：uprobe_list 锁内——跑 handler + 取首实例 XOL slot 偏移 ──
    let xol_slot_offset = {
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
        // rip 保持 raw（probe_vaddr+1）：BPF 回调经 break_address()=rip-1 取得原探针址。
        // （镜像 kprobe 持锁跑 callback；callback 为 JIT eBPF，不睡眠。）
        for entry in entries {
            let inst = entry.read();
            if inst.basic.is_enabled() {
                inst.basic.call_pre_handler(frame);
                inst.basic.call_event_callback(frame);
            }
        }
        // XOL 只需执行一次原指令（同址各实例指令相同）；取首个实例的 slot。
        // slot 内容已在注册时预填（uprobe_register 调 build_xol_slot），命中时直接用。
        let inst0 = entries[0].read();
        inst0
            .basic
            .probe_point()
            .ok_or(SystemError::EINVAL)?
            .xol_slot_offset
    }; // uprobe_list 释放

    // ── Phase 2：xol_area 锁内——取 slot 用户址（slot 已预填，无需再写）──
    let slot_vaddr = {
        let guard = mm.xol_area.lock_irqsave();
        let area = guard.as_ref().ok_or(SystemError::EFAULT)?;
        area.slot_vaddr(xol_slot_offset)
    };

    // ── Phase 3：重定向 rip→XOL slot + 置 TF + 置 NEED_UPROBE ──
    frame.set_rip(slot_vaddr.data());
    frame.rflags |= RFLAGS_TF;
    pcb.flags().insert(ProcessFlags::NEED_UPROBE);

    Ok(())
}

/// 用户态 #DB 分发：`NEED_UPROBE` 置位 → XOL 单步完成，否则为 ptrace/硬件断点。
///
/// 调用方（`do_debug`）已保证 `is_from_user()` 为真。
pub fn uprobe_debug_handler(frame: &mut TrapFrame) -> Result<(), SystemError> {
    let pcb = ProcessManager::current_pcb();

    // ── F4：NEED_UPROBE 是 #DB 判别位 ──
    if !pcb.flags().contains(ProcessFlags::NEED_UPROBE) {
        // 非 uprobe 单步 #DB：ptrace 单步 / 硬件断点。
        // 阶段一不处理（步骤 6 文档化）；不投递信号以免扰乱 ptrace 自有路径。
        debug!(
            "user #DB without NEED_UPROBE @ rip {:#x} (ptrace/hw-bp, deferred)",
            frame.rip
        );
        return Ok(());
    }

    // 清 NEED_UPROBE：识别为 XOL 单步完成。
    pcb.flags().remove(ProcessFlags::NEED_UPROBE);

    let mm = pcb.basic().user_vm().ok_or(SystemError::EFAULT)?;

    // 由 NEED_UPROBE 置位保证 frame.rip 落在 XOL 页内。XOL slot 16 字节对齐，
    // 故 `slot_offset = (rip - page_base) & !0xF` 可无 insn_len 反算 slot 偏移。
    let page_base = {
        let guard = mm.xol_area.lock_irqsave();
        guard.as_ref().ok_or(SystemError::EFAULT)?.page_base()
    };
    let slot_offset =
        (frame.rip as usize).saturating_sub(page_base.data()) & !(UPROBE_INSN_COPY_SIZE - 1);

    // ── uprobe_list 锁内——由 slot 反查探针址、跑 post_handler、定返回址 ──
    let return_addr = {
        let list = mm.uprobe_list.lock_irqsave();
        // slot 偏移唯一标识一个实例（bitmap 分配），据此反查 probe_vaddr。
        let hit_vaddr = find_probe_vaddr_by_slot(&list, slot_offset);
        let Some(probe_vaddr) = hit_vaddr else {
            // 极端竞态：#BP 后、#DB 前该 uprobe 被注销（slot 已释放）。
            warn!(
                "uprobe #DB: NEED_UPROBE set but slot {:#x} (rip {:#x}) unmatched — racy unregister?",
                slot_offset,
                frame.rip
            );
            // 无法恢复 probe_vaddr；清 TF 防止单步循环。
            frame.rflags &= !RFLAGS_TF;
            return Ok(());
        };
        let Some(entries) = list.get(&probe_vaddr) else {
            frame.rflags &= !RFLAGS_TF;
            return Ok(());
        };
        // 原指令长度对所有同址实例一致；取首实例的 return_address。
        let return_addr = entries[0]
            .read()
            .basic
            .probe_point()
            .ok_or(SystemError::EINVAL)?
            .return_address();
        // F5：post_handler 必须在原程序语境运行——先把 rip 改回返回址，再跑。
        // event_callback（BPF）仅在 #BP 命中时触发一次，#DB 不再二次投递（计划步骤5）。
        frame.set_rip(return_addr);
        for entry in entries {
            let inst = entry.read();
            if inst.basic.is_enabled() {
                inst.basic.call_post_handler(frame);
            }
        }
        return_addr
    }; // uprobe_list 释放

    // rip 已在锁内改回返回址；此处清 TF（参照 kprobe.rs clear_single_step）。
    frame.rflags &= !RFLAGS_TF;
    debug!("uprobe XOL single-step done: resume {:#x}", return_addr);

    Ok(())
}

/// 在 uprobe_list 中找到 `xol_slot_offset == slot_offset` 的探针地址。
///
/// slot 偏移由 `XolArea::alloc_slot` 的 bitmap 唯一分配，故至多匹配一个实例。
fn find_probe_vaddr_by_slot(
    list: &BTreeMap<usize, Vec<Arc<RwLock<UprobeInstance>>>>,
    slot_offset: usize,
) -> Option<usize> {
    for (vaddr, entries) in list.iter() {
        for entry in entries {
            if let Some(pp) = entry.read().basic.probe_point() {
                if pp.xol_slot_offset == slot_offset {
                    return Some(*vaddr);
                }
            }
        }
    }
    None
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
