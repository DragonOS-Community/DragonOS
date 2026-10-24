//! Terminal handling of exhausted infallible allocations.
//!
//! This is not OOM recovery. The allocation call cannot return a value, and
//! its caller may hold arbitrary locks. Do not reclaim, unwind, schedule, or
//! enter the normal logging, clock, console, or shutdown infrastructure here.

use core::{
    alloc::Layout,
    fmt::Write,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{arch::CurrentIrqArch, exception::InterruptArch};

static FATAL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Consulted by the early stop-IPI entry before any task or IRQ bookkeeping.
pub(crate) fn is_active() -> bool {
    FATAL_ACTIVE.load(Ordering::Acquire)
}

/// Allocation-free final failure of an infallible allocation, not a task exit.
/// Only the first CPU attempts a bounded diagnostic and peer-stop request.
#[cold]
pub(crate) fn allocation_failure(layout: Layout) -> ! {
    unsafe { CurrentIrqArch::interrupt_disable() };
    if !FATAL_ACTIVE.swap(true, Ordering::AcqRel) {
        #[cfg(target_arch = "x86_64")]
        crate::arch::driver::apic::try_fatal_stop_others();

        // Only static text and integer formatting: arbitrary Display code is
        // intentionally excluded from this emergency reporting contract.
        let mut record = crate::libs::printk::BoundedLogRecord::new();
        let _ = writeln!(
            record,
            "[fatal-alloc] infallible allocation failed: size={} align={}; kernel terminating",
            layout.size(),
            layout.align()
        );
        report_bytes(record.as_bytes());
    }
    halt_current_cpu()
}

/// No scheduler state transition, locks, allocation, or normal stop handler.
/// IRQs remain disabled permanently; this CPU never resumes the failed task.
#[cold]
pub(crate) fn halt_current_cpu() -> ! {
    unsafe { CurrentIrqArch::interrupt_disable() };
    loop {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack))
        };
        #[cfg(not(target_arch = "x86_64"))]
        core::hint::spin_loop();
    }
}

#[cfg(target_arch = "x86_64")]
fn report_bytes(bytes: &[u8]) {
    // COM1 is the bootstrap diagnostic UART. Bypass driver ownership and
    // clocks: terminal diagnostics may be lost/interleaved, never block on
    // a lock held by the failed allocation. An absent UART is harmless.
    for &byte in bytes {
        let mut ready = false;
        for _ in 0..1024 {
            if unsafe { x86::io::inb(0x3fd) } & 0x20 != 0 {
                ready = true;
                break;
            }
            core::hint::spin_loop();
        }
        if !ready {
            return;
        }
        unsafe { x86::io::outb(0x3f8, byte) };
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn report_bytes(_bytes: &[u8]) {
    // No bounded raw firmware/UART sink is available on these backends.
    // Do not substitute a potentially blocking firmware call or normal logger.
}
