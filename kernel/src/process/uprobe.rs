//! Per-task uprobe XOL execution state.
//!
//! This module owns only the task-local lifecycle of an instruction executing
//! out of line. Architecture exception code remains responsible for trap-frame
//! interpretation, DR6 handling, and signal delivery.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU8, Ordering};

use crate::libs::spinlock::SpinLock;
use crate::mm::ucontext::XolSlotLease;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskXolPhase {
    Idle = 0,
    Running = 1,
    Trapped = 2,
}

impl TaskXolPhase {
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Idle,
            1 => Self::Running,
            2 => Self::Trapped,
            _ => unreachable!("invalid task XOL phase"),
        }
    }
}

/// The resources and original execution context retained while one task is
/// executing an instruction from an XOL slot.
pub struct ActiveXol {
    pub probe_vaddr: usize,
    pub return_addr: usize,
    pub orig_tf: bool,
    pub slot_end: usize,
    pub xol_lease: Arc<XolSlotLease>,
}

impl core::fmt::Debug for ActiveXol {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ActiveXol")
            .field("probe_vaddr", &self.probe_vaddr)
            .field("return_addr", &self.return_addr)
            .field("orig_tf", &self.orig_tf)
            .field("slot_end", &self.slot_end)
            .field("xol_slot_offset", &self.xol_lease.offset())
            .finish_non_exhaustive()
    }
}

/// One live uretprobe hijack owned by this task: the original return address
/// that was replaced on the user stack by the mm-wide trampoline, together
/// with the retprobe participants snapshot to deliver on return.
pub struct ActiveUret {
    pub orig_ret_addr: usize,
    pub participants: Arc<crate::mm::ucontext::UprobeParticipantNode>,
}

impl core::fmt::Debug for ActiveUret {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ActiveUret")
            .field("orig_ret_addr", &self.orig_ret_addr)
            .finish_non_exhaustive()
    }
}

/// Single source of truth for one task's XOL state.
///
/// `phase` is the lock-free exception-routing discriminator. `payload` owns
/// the lease and context. Publication stores the payload before releasing the
/// Running phase; consumers acquire/swap the phase before taking the payload.
pub struct TaskXolState {
    phase: AtomicU8,
    payload: SpinLock<Option<ActiveXol>>,
    /// Live uretprobe return instances, innermost last. LIFO on the trampoline
    /// hit keeps nested hijacks of the same function correct.
    ret_stack: SpinLock<alloc::vec::Vec<ActiveUret>>,
}

impl core::fmt::Debug for TaskXolState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TaskXolState")
            .field("phase", &self.phase())
            .finish_non_exhaustive()
    }
}

impl TaskXolState {
    pub const fn new() -> Self {
        Self {
            phase: AtomicU8::new(TaskXolPhase::Idle as u8),
            payload: SpinLock::new(None),
            ret_stack: SpinLock::new(alloc::vec::Vec::new()),
        }
    }

    /// Record one uretprobe hijack. Called from process context on the
    /// entry-path after the user stack slot has been rewritten.
    pub fn push_uret(&self, instance: ActiveUret) {
        let mut stack = self.ret_stack.lock_irqsave();
        let _ = stack.try_reserve_exact(1);
        stack.push(instance);
    }

    /// Pop the innermost uretprobe hijack on the trampoline hit. Returns the
    /// instance so the handler can restore its original return address.
    pub fn pop_uret(&self) -> Option<ActiveUret> {
        self.ret_stack.lock_irqsave().pop()
    }

    /// Drop every live hijack without unwinding user stacks: used when the
    /// old user context is being discarded (exec) so stale trampoline
    /// addresses never reference a freed address space.
    pub fn clear_urets(&self) {
        self.ret_stack.lock_irqsave().clear();
    }

    #[inline]
    pub fn phase(&self) -> TaskXolPhase {
        TaskXolPhase::from_raw(self.phase.load(Ordering::Acquire))
    }

    /// Return the original userspace instruction address represented by an
    /// active XOL operation.
    ///
    /// Architecture code uses this logical address when a subsystem such as
    /// rseq must reason about the interrupted userspace instruction rather
    /// than the private XOL slot currently stored in the trap frame.
    pub fn active_probe_vaddr(&self) -> Option<usize> {
        if self.phase() == TaskXolPhase::Idle {
            return None;
        }

        self.payload
            .lock_irqsave()
            .as_ref()
            .map(|active| active.probe_vaddr)
    }

    /// Publish a newly active XOL operation. Refuses to overwrite an existing
    /// lease, which would otherwise make the earlier instruction unrecoverable.
    pub fn publish_running(&self, active: ActiveXol) -> Result<(), ActiveXol> {
        let mut payload = self.payload.lock_irqsave();
        if self.phase.load(Ordering::Acquire) != TaskXolPhase::Idle as u8 || payload.is_some() {
            return Err(active);
        }
        *payload = Some(active);
        drop(payload);
        self.phase
            .store(TaskXolPhase::Running as u8, Ordering::Release);
        Ok(())
    }

    /// Mark the active instruction as having raised a synchronous trap.
    /// Repeated calls are idempotent.
    pub fn mark_trapped(&self) -> Option<usize> {
        loop {
            match self.phase() {
                TaskXolPhase::Idle => return None,
                TaskXolPhase::Trapped => break,
                TaskXolPhase::Running => {
                    if self
                        .phase
                        .compare_exchange(
                            TaskXolPhase::Running as u8,
                            TaskXolPhase::Trapped as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        break;
                    }
                }
            }
        }
        self.payload
            .lock_irqsave()
            .as_ref()
            .map(|active| active.probe_vaddr)
    }

    /// Atomically return to Idle and take the active payload. The returned old
    /// phase is the authoritative classification for #DB completion.
    pub fn take(&self) -> Option<(TaskXolPhase, ActiveXol)> {
        let phase =
            TaskXolPhase::from_raw(self.phase.swap(TaskXolPhase::Idle as u8, Ordering::AcqRel));
        if phase == TaskXolPhase::Idle {
            return None;
        }
        let active = self.payload.lock_irqsave().take();
        debug_assert!(active.is_some(), "active XOL phase without payload");
        active.map(|active| (phase, active))
    }

    /// Drop an active XOL operation without modifying an obsolete trap frame,
    /// as required by successful exec and exit.
    pub fn discard(&self) -> bool {
        let active = self.take();
        let existed = active.is_some();
        drop(active);
        existed
    }
}

impl Default for TaskXolState {
    fn default() -> Self {
        Self::new()
    }
}
