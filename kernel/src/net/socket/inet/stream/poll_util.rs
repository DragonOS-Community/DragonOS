use crate::process::ProcessManager;
use crate::sched;

/// Number of consecutive `Iface::poll()` rounds per batch in a syscall fast-path.
///
/// Rationale:
/// - `Iface::poll()` typically returns `true` only while there is immediate work to do.
/// - However, in pathological cases (e.g. a very large backlog), an unbounded `while poll() {}`
///   tight loop can cause long syscall latency and starve other tasks.
///
/// We poll in batches and:
/// - periodically check pending (unmasked) signals so higher-level syscall code can
///   return EINTR when appropriate;
/// - call `sched_yield()` between batches to avoid monopolizing the CPU.
pub(super) const IFACE_POLL_BATCH_ROUNDS: usize = 128;

/// Poll the interface until quiescent, with signal-aware early exit and cooperative yielding.
#[inline]
pub(super) fn poll_iface_until_quiescent(iface: &dyn crate::net::Iface) {
    loop {
        let mut progressed = false;
        for i in 0..IFACE_POLL_BATCH_ROUNDS {
            if !iface.poll() {
                return;
            }
            progressed = true;

            // Avoid checking signals on every iteration; keep it cheap.
            if (i & 0x7) == 0x7 {
                let pcb = ProcessManager::current_pcb();
                if pcb.has_pending_signal_fast() && pcb.has_pending_not_masked_signal() {
                    return;
                }
            }
        }

        // If we keep making progress, yield so other tasks get CPU time.
        // This also helps keep syscall latency bounded without relying on a background poller.
        if progressed {
            sched::sched_yield();
        } else {
            return;
        }
    }
}
