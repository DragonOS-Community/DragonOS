use crate::sched;

/// Number of consecutive `Iface::poll()` rounds per batch in a syscall fast-path.
///
/// Rationale:
/// - `Iface::poll()` typically returns `true` only while there is immediate work to do.
/// - However, in pathological cases (e.g. a very large backlog), an unbounded `while poll() {}`
///   tight loop can cause long syscall latency and starve other tasks.
///
/// We poll in batches and call `sched_yield()` between batches to avoid
/// monopolizing the CPU.
///
/// Important:
/// signal interruption must be handled at the actual blocking wait sites
/// (`wait_event_*`, poll/epoll waits, etc.), not here. If we stop protocol
/// progress early just because a signal is pending, callers may observe a
/// transient "not writable yet" state and incorrectly fall back to sleeping or
/// short-write behavior before loopback ACK/window updates have been fully
/// processed. Linux `tcp_sendmsg()` only converts signals into EINTR/short-write
/// at its real wait points; the fast-path protocol progress itself is not
/// prematurely aborted.
pub(super) const IFACE_POLL_BATCH_ROUNDS: usize = 128;

/// Poll the interface until quiescent, with cooperative yielding.
#[inline]
pub(super) fn poll_iface_until_quiescent(iface: &dyn crate::net::Iface) {
    loop {
        let mut progressed = false;
        for _ in 0..IFACE_POLL_BATCH_ROUNDS {
            if !iface.poll() {
                return;
            }
            progressed = true;
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
