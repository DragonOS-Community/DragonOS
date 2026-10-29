//! Per-receive remaining wait budget, shared across protocol retries.

use system_error::SystemError;

use crate::libs::wait_queue::WaitQueue;
use crate::time::{Duration, Instant};

/// Match Linux's sock_intr_errno(): only an infinite socket wait may restart.
/// The caller supplies the timeout captured for this operation, rather than a
/// socket option that another thread may have changed while it was waiting.
pub(crate) fn map_wait_error(error: SystemError, finite: bool) -> SystemError {
    match error {
        SystemError::ERESTARTSYS if finite => SystemError::EINTR,
        error => error,
    }
}

/// Snapshot SO_RCVTIMEO once per operation. Only time spent in the wait
/// primitive is charged, matching Linux's remaining schedule_timeout budget.
/// Protocol owners retain responsibility for trying receive before waiting and
/// for returning any bytes already received before a wait fails.
pub(crate) struct ReceiveTimeout {
    remaining: Option<Duration>,
}

impl ReceiveTimeout {
    pub(crate) fn new(timeout: Option<Duration>) -> Self {
        Self { remaining: timeout }
    }

    pub(crate) fn wait<F>(&mut self, queue: &WaitQueue, condition: F) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
    {
        self.wait_with(|timeout| queue.wait_event_interruptible_timeout(condition, timeout))
    }

    pub(crate) fn wait_io<F>(&mut self, queue: &WaitQueue, condition: F) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
    {
        self.wait_with(|timeout| queue.wait_event_io_interruptible_timeout(condition, timeout))
    }

    fn wait_with<F>(&mut self, wait: F) -> Result<(), SystemError>
    where
        F: FnOnce(Option<Duration>) -> Result<(), SystemError>,
    {
        let Some(remaining) = self.remaining else {
            return wait(None);
        };
        // The owner has already attempted real receive. An expired budget must
        // not be extended by a stale poll-readiness hint in the wait predicate.
        if remaining == Duration::ZERO {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        let started = Instant::now();
        let result = wait(Some(remaining));
        let elapsed = Instant::now().saturating_sub(started);
        self.remaining = Some(Duration::from_micros(
            remaining
                .total_micros()
                .saturating_sub(elapsed.total_micros()),
        ));
        // Finite socket timeouts interrupt with EINTR even with SA_RESTART.
        // Restarting the syscall would otherwise create a fresh timeout budget.
        result.map_err(|error| map_wait_error(error, true))
    }
}
