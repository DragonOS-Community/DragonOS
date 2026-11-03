//! Re-publish a normal migration notification inside the real faultable handler.
//! This tests notification ownership, without pretending a CPU migration occurred.

use alloc::{string::String, sync::Arc};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, CurrentIrqArch},
    exception::InterruptArch,
    process::{ProcessControlBlock, ProcessFlags, ProcessManager},
};

use super::Rseq;

static OWNER: AtomicUsize = AtomicUsize::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);

struct Probe;

impl Drop for Probe {
    fn drop(&mut self) {
        ARMED.store(false, Ordering::Relaxed);
        OWNER.store(0, Ordering::Release);
    }
}

pub(super) fn after_cpu_id_capture(pcb: &Arc<ProcessControlBlock>) {
    let tag = pcb.raw_pid().data() + 1;
    if OWNER.load(Ordering::Acquire) == tag && ARMED.swap(false, Ordering::AcqRel) {
        // Use the scheduler's event producer, not a raw process-flag store.
        Rseq::on_migrate(pcb);
    }
}

pub(crate) fn run() -> Result<String, SystemError> {
    let pcb = ProcessManager::current_pcb();
    if !pcb.rseq_state().is_registered() {
        return Err(SystemError::EINVAL);
    }
    OWNER
        .compare_exchange(
            0,
            pcb.raw_pid().data() + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map_err(|_| SystemError::EBUSY)?;
    let _probe = Probe;
    ARMED.store(true, Ordering::Release);

    // Match the return boundary's notification consumption and IRQ context.
    // The user registration remains installed and both calls access real user
    // memory. No locks cover those accesses.
    let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
    pcb.flags().remove(ProcessFlags::NEED_RSEQ);
    unsafe { CurrentIrqArch::interrupt_enable() };
    let first = Rseq::handle_notify_resume::<TrapFrame>(None);
    unsafe { CurrentIrqArch::interrupt_disable() };
    let injected = !ARMED.load(Ordering::Acquire);
    let preserved = pcb.flags().test_and_clear(ProcessFlags::NEED_RSEQ);
    unsafe { CurrentIrqArch::interrupt_enable() };
    let second = Rseq::handle_notify_resume::<TrapFrame>(None);
    unsafe { CurrentIrqArch::interrupt_disable() };
    drop(irq_guard);

    // A real preemption during the second pass may leave another notification;
    // do not consume it or require the final flag to be empty.
    let ok = injected && preserved && first.is_ok() && second.is_ok();
    Ok(alloc::format!(
        "status={}\nmigration_notification_injected={}\nnotification_preserved={}\nhandler_passes=2\n",
        if ok { "ok" } else { "fail" },
        injected,
        preserved,
    ))
}
