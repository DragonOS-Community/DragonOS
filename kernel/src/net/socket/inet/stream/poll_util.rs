/// Number of consecutive `TcpStack::poll()` rounds per batch in a syscall fast-path.
///
/// Rationale:
/// - `TcpStack::poll()` typically returns `true` only while there is immediate work to do.
/// - However, in pathological cases (e.g. a very large backlog), an unbounded `while poll() {}`
///   tight loop can cause long syscall latency and starve other tasks.
///
/// Only a finite batch belongs to a syscall. Other connections in the same
/// namespace may remain active indefinitely; their traffic must not delay
/// this socket's nonblocking, signal or timeout checks.
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
pub(super) const STACK_POLL_BATCH_ROUNDS: usize = 128;

/// Help the transport for a finite batch, then leave remaining work to its worker.
#[inline]
pub(super) fn poll_stack_batch(stack: &crate::net::tcp_stack::TcpStack) {
    for _ in 0..STACK_POLL_BATCH_ROUNDS {
        if !stack.poll() {
            return;
        }
    }
    stack.request_poll();
}
