//! uprobe user-mode exception dispatch (planned steps 5/6/8).
//!
//! `do_int3`/`do_debug` branch on [`TrapFrame::is_from_user`]: user mode goes through
//! this module, kernel mode goes through the existing kprobe dispatch
//! ([`crate::exception::ebreak::EBreak`] / [`crate::exception::debug::DebugException`]).
//!
//! # Hit-path constraints (interrupts disabled, entry.S `cli`)
//!
//! - Only `lock_irqsave` + table lookup + trapframe modification + run BPF (callback);
//! - Never touch page tables, never sleep, never take `AddressSpace`'s `RwSem` (it can sleep);
//! - XOL slot contents are written through the kernel direct-map of the physical page
//!   (`phys_2_virt`), without needing `PageMapper`/`RwSem` — that is the design intent of
//!   `XolPage::page_paddr`.
//!
//! # F5: BPF transparency
//!
//! The event callback entry rip must be
//! the original probe context (`probe_vaddr` or `probe_vaddr + insn_len`); the XOL slot
//! user address is **never** exposed to BPF.

use crate::arch::interrupt::TrapFrame;
use crate::arch::ipc::signal::Signal;
use crate::arch::CurrentIrqArch;
use crate::exception::InterruptArch;
use crate::ipc::signal::force_sig_fault_to_current;
use crate::mm::VirtAddr;
use crate::process::uprobe::{ActiveXol, TaskXolPhase};
use crate::process::{ProcessControlBlock, ProcessFlags, ProcessManager};
use kprobe::ProbeArgs;
use log::{debug, warn};
use system_error::SystemError;

/// The `si_code` of `SIGTRAP`: breakpoint trap (mirrors Linux `TRAP_BRKPT`, used for
/// unconsumed user-mode #BP).
const TRAP_BRKPT: i32 = 1;

/// The TF (Trap Flag) bit of RFLAGS — once set, a #DB is triggered after every
/// instruction, used for XOL single-stepping.
const RFLAGS_TF: u64 = 1 << 8;

/// User-mode #BP (int3) dispatch: if a uprobe matches, XOL single-steps the original
/// instruction; otherwise it delivers `SIGTRAP(TRAP_BRKPT)`.
///
/// The caller (`do_int3`) has guaranteed that `is_from_user()` is true.
pub fn uprobe_breakpoint_handler(frame: &mut TrapFrame) -> Result<(), SystemError> {
    let break_addr = frame.break_address(); // = rip - 1 = probe_vaddr (raw rip is preserved,
                                            // for BPF callbacks to retrieve the original probe address via break_address() — the batch3↔batch4 runtime contract).

    // F5: during callback execution rip must stay raw (probe_vaddr+1); never preset it
    // to the original probe address here, otherwise `break_address()=rip-1` inside the
    // callback would yield probe_vaddr-1 instead of probe_vaddr.
    // The XOL slot user address is written to rip only after all callbacks return; it is
    // never exposed to BPF.

    let pcb = ProcessManager::current_pcb();
    let mm = match pcb.basic().user_vm() {
        Some(mm) => mm,
        None => {
            // User-mode #BP without a user_vm (should not happen): defensively deliver SIGTRAP.
            return send_sigtrap_brkpt(frame, break_addr);
        }
    };

    // ── Phase 1: copy IRQ-safe runtime values inside the RCU hit snapshot ──
    let captured = mm
        .uprobe_list
        .with_hit(break_addr, |site| -> Result<_, SystemError> {
            let xol_lease = site.xol_lease.clone();
            let slot_vaddr = xol_lease.slot_vaddr().data();
            let slot_end = slot_vaddr
                .checked_add(site.insn_analysis.insn_len)
                .ok_or(SystemError::EINVAL)?;
            let return_addr = site
                .probe_vaddr
                .checked_add(site.insn_analysis.insn_len)
                .ok_or(SystemError::EINVAL)?;

            let participants = site.participant_snapshot();
            Ok((xol_lease, slot_vaddr, slot_end, return_addr, participants))
        });
    let Some(captured) = captured else {
        return send_sigtrap_brkpt(frame, break_addr);
    };
    let (xol_lease, slot_vaddr, slot_end, return_addr, participants) = captured?;

    // #BP uses a DPL=3 interrupt gate so teardown cannot observe this CPU's
    // shootdown acknowledgement before the hit-table lookup and slot lease
    // capture are complete. The XOL VMA is immutable, so callbacks may run
    // from this point with normal user-exception interrupt semantics.
    unsafe { CurrentIrqArch::interrupt_enable() };

    // ── Phase 1.5: run event callbacks outside the lock (review R12) ──
    // rip stays raw (probe_vaddr+1): BPF callbacks get the original probe address via break_address()=rip-1.
    if let Some(participants) = participants {
        participants.for_each_active(|participant| participant.deliver(&pcb, frame));
    }

    // ── Phase 2: save per-thread active state (reviews R2/R5) → redirect rip → set TF ──
    let orig_tf = frame.rflags & RFLAGS_TF != 0;
    let active = ActiveXol {
        probe_vaddr: break_addr,
        return_addr,
        orig_tf,
        slot_end,
        xol_lease,
    };
    if let Err(active) = pcb.uprobe.publish_running(active) {
        warn!(
            "nested uprobe XOL state at probe {:#x}; preserving existing lease",
            active.probe_vaddr
        );
        return send_sigtrap_brkpt(frame, break_addr);
    }
    frame.set_rip(slot_vaddr);
    frame.rflags |= RFLAGS_TF;

    Ok(())
}

/// User-mode #DB dispatch: if the task's XOL state is active, the XOL single-step has
/// completed (consumed); otherwise this exception is **not uprobe-related** and `false`
/// is returned so the caller routes it to the normal debug path (ptrace single-step /
/// hardware breakpoint / SIGTRAP, review R4).
///
/// The caller (`do_debug`) has guaranteed that `is_from_user()` is true.
///
/// The #DB entry owns signal conversion. This result only states which part
/// of the hardware cause the XOL machinery consumed.
#[derive(Debug, Clone, Copy)]
pub struct UprobeDebugOutcome {
    pub consumed: bool,
    pub report_single_step: bool,
}

impl UprobeDebugOutcome {
    const NOT_CONSUMED: Self = Self {
        consumed: false,
        report_single_step: false,
    };
}

pub fn uprobe_debug_handler(
    frame: &mut TrapFrame,
    dr6: u64,
) -> Result<UprobeDebugOutcome, SystemError> {
    let pcb = ProcessManager::current_pcb();

    if pcb.uprobe.phase() == TaskXolPhase::Idle {
        // A non-uprobe single-step #DB: do not consume it (review R4); hand it back to
        // do_debug for the normal DebugException path (ptrace / hardware breakpoint / SIGTRAP).
        return Ok(UprobeDebugOutcome::NOT_CONSUMED);
    }

    // Atomically return the task state to Idle and preserve the old phase for
    // exact Running/Trapped classification.
    let state = pcb.uprobe.take();
    let Some((phase, state)) = state else {
        warn!(
            "uprobe #DB: active phase without payload @ rip {:#x}",
            frame.rip
        );
        return Ok(UprobeDebugOutcome::NOT_CONSUMED);
    };

    // Only when the Running original instruction finishes exactly inside this lease's
    // slot is this the XOL completion #DB. Some virtualization environments do not report
    // DR6.BS for this exact completion event; the attribution is decided by the task's XOL
    // phase and the exact slot endpoint. DR6.B0-B3 are still handed over to normal hardware
    // breakpoint semantics after completion. A page-range check would mistake a redirected
    // #DB for a completion.
    let rip = frame.rip as usize;
    if phase != TaskXolPhase::Running || rip != state.slot_end {
        warn!(
            "uprobe #DB abort: rip {:#x}, expected {:#x}, dr6 {:#x}, state {:?}, re-execute probe {:#x}",
            rip, state.slot_end, dr6, phase, state.probe_vaddr
        );
        restore_after_abort(frame, &state);
        pcb.recalc_sigpending();
        // The current #DB is not an XOL completion event; hand it to the user debug/SIGTRAP path.
        return Ok(UprobeDebugOutcome::NOT_CONSUMED);
    }

    // ── XOL completion: restore rip to the return address + restore the original TF (review R5) ──
    frame.set_rip(state.return_addr);
    if state.orig_tf {
        // This XOL was the single instruction the debugger asked to single-step. Linux
        // arch_uprobe_post_xol() queues SIGTRAP immediately here; if we merely kept TF and
        // waited for the next #DB, an extra real instruction would execute before the
        // debugger is notified.
        frame.rflags |= RFLAGS_TF;
    } else {
        frame.rflags &= !RFLAGS_TF;
    }

    debug!(
        "uprobe XOL single-step done: resume {:#x} (orig_tf={})",
        state.return_addr, state.orig_tf
    );

    // Re-publish ordinary pending signals that were temporarily deferred during the XOL window.
    pcb.recalc_sigpending();

    Ok(UprobeDebugOutcome {
        consumed: true,
        // If TF predated XOL, the completed instruction is still a real user
        // single-step and must be reported by the unified deferred #DB path.
        report_single_step: state.orig_tf,
    })
}

/// Deliver `SIGTRAP(TRAP_BRKPT)` to the current process (an unconsumed user-mode #BP).
///
/// Following the pattern of `do_undefined_opcode` sending `SIGILL`: enable interrupts
/// first, then deliver the signal. `si_addr` is set to the breakpoint address.
fn send_sigtrap_brkpt(frame: &mut TrapFrame, break_addr: usize) -> Result<(), SystemError> {
    // If this #BP came from the XOL instruction itself, it is a synchronous exception
    // that will definitely deliver a signal.
    mark_current_xol_trapped();
    // #Safety: `interrupt_enable` only sets RFLAGS.IF; we are already out of the uprobe
    // hit path's irqsave critical section (the uprobe_list/XOL slot locks are released),
    // consistent with do_undefined_opcode.
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

/// Mark the current XOL as trapped. Idempotent; should only be called after a synchronous
/// exception is certain to deliver a signal.
pub fn mark_current_xol_trapped() -> bool {
    mark_current_xol_trapped_and_get_probe_addr().is_some()
}

/// Mark the current XOL instruction as trapped and return the original
/// instruction address. Synchronous fault handlers use this to build siginfo
/// without exposing the private XOL slot address to userspace.
pub fn mark_current_xol_trapped_and_get_probe_addr() -> Option<usize> {
    let pcb = ProcessManager::current_pcb();
    pcb.uprobe.mark_trapped()
}

/// Idempotently abort the current XOL and restore the trapframe to a state where the
/// original instruction can be retried.
pub fn abort_current_xol(frame: &mut TrapFrame) -> bool {
    let pcb = ProcessManager::current_pcb();
    let state = pcb.uprobe.take();
    let Some((_phase, state)) = state else {
        return false;
    };
    restore_after_abort(frame, &state);
    drop(state);
    pcb.recalc_sigpending();
    true
}

/// Paths such as exec/exit that will not return to the old user context can call this to
/// release ActiveXol's strong references to site/slot/consumer. There is no need — and no
/// requirement — to modify a trapframe that is about to be discarded.
pub fn cleanup_task_active_xol(pcb: &ProcessControlBlock) {
    pcb.uprobe.discard();
    pcb.recalc_sigpending();
}

/// Signal delivery gate: returning `false` means an ordinary asynchronous signal must be
/// deferred until this XOL instruction completes. A fatal path, or one already marked
/// Trapped, aborts first and then allows the signal to build the user frame.
pub fn signal_gate(frame: &mut TrapFrame) -> bool {
    let pcb = ProcessManager::current_pcb();
    let state = pcb.uprobe.phase();
    if state == TaskXolPhase::Idle {
        return true;
    }

    // group-exit and thread/shared SIGKILL must terminate XOL immediately. Although this
    // helper is named for OOM, its semantics are exactly the full fatal-pending query
    // needed here.
    let fatal = Signal::oom_fatal_signal_pending(&pcb);
    if state == TaskXolPhase::Running && !fatal {
        // The pending queue stays unchanged; only the fast flag is temporarily cleared to
        // avoid re-entering the exit-to-user loop in place. The immediately following #DB
        // completion/abort of the XOL will recalc and re-set it.
        pcb.flags().remove(ProcessFlags::HAS_PENDING_SIGNAL);
        // Re-check once against concurrent signal senders: if a fatal arrived between the
        // first query and clearing the flag, it must abort now; if it arrives after this
        // query, the sender will re-set HAS_PENDING_SIGNAL, and the exit-to-user loop will
        // re-enter this gate.
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
