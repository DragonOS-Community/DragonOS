//! Per-socket accounting for complete datagrams owned by the IPv4 output queue.
//!
//! The queue only retains this account, not the socket or its smoltcp stack.
//! Its weak back-reference is used solely to wake blocked writers after a
//! previously admitted packet has left the queue.

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicUsize, Ordering};
use system_error::SystemError;

use crate::{
    driver::net::local_queue::{OutputCharge, OutputCompletion},
    filesystem::{
        epoll::{event_poll::EventPoll, EPollEventType as EP},
        vfs::fasync::FASYNC_POLL_OUT,
    },
    net::socket::Socket,
};

#[derive(Debug)]
pub(crate) struct SocketOutputAccount<S: Socket + ?Sized> {
    socket: Weak<S>,
    charged: AtomicUsize,
    limit: AtomicUsize,
}

impl<S: Socket + ?Sized> SocketOutputAccount<S> {
    pub(crate) fn new(socket: Weak<S>, limit: usize) -> Self {
        Self {
            socket,
            charged: AtomicUsize::new(0),
            limit: AtomicUsize::new(limit),
        }
    }

    pub(crate) fn is_writable(&self) -> bool {
        self.charged.load(Ordering::Acquire) < self.limit.load(Ordering::Acquire) / 2
    }

    pub(crate) fn available(&self) -> usize {
        self.limit
            .load(Ordering::Acquire)
            .saturating_sub(self.charged.load(Ordering::Acquire))
    }

    pub(crate) fn can_charge(&self, bytes: usize) -> bool {
        let charged = self.charged.load(Ordering::Acquire);
        charged == 0
            || charged
                .checked_add(bytes)
                .is_some_and(|total| total <= self.limit.load(Ordering::Acquire))
    }

    pub(crate) fn set_limit(&self, limit: usize) {
        let was_writable = self.is_writable();
        self.limit.store(limit, Ordering::Release);
        if !was_writable && self.is_writable() {
            self.wake_writers();
        } else {
            self.wake_blocked_writers();
        }
    }

    /// Linux can admit one packet larger than the poll writable threshold.
    /// The interface reservation still bounds total pending packet memory.
    pub(crate) fn charge(self: &Arc<Self>, bytes: usize) -> Result<OutputCharge, SystemError>
    where
        S: 'static,
    {
        let mut old = self.charged.load(Ordering::Acquire);
        loop {
            let new = old.checked_add(bytes).ok_or(SystemError::ENOBUFS)?;
            let limit = self.limit.load(Ordering::Acquire);
            if old != 0 && new > limit {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            match self
                .charged
                .compare_exchange_weak(old, new, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    let owner: Arc<dyn OutputCompletion> = self.clone();
                    return Ok(OutputCharge::new(owner, bytes));
                }
                Err(observed) => old = observed,
            }
        }
    }

    fn wake_writers(&self) {
        let Some(socket) = self.socket.upgrade() else {
            return;
        };
        socket.wait_queue().wakeup_all(None);
        let _ = EventPoll::wakeup_epoll(
            socket.epoll_items().as_ref(),
            EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND,
        );
        socket.fasync_items().send_sigio(FASYNC_POLL_OUT);
    }

    fn wake_blocked_writers(&self) {
        if let Some(socket) = self.socket.upgrade() {
            socket.wait_queue().wakeup_all(None);
        }
    }
}

impl<S: Socket + ?Sized> OutputCompletion for SocketOutputAccount<S> {
    fn output_complete(&self, charged_bytes: usize) {
        let old = self.charged.fetch_sub(charged_bytes, Ordering::AcqRel);
        debug_assert!(old >= charged_bytes);
        let remaining = old - charged_bytes;
        let threshold = self.limit.load(Ordering::Acquire) / 2;
        if old >= threshold && remaining < threshold {
            self.wake_writers();
        } else {
            // A large send can be blocked even while POLLOUT is true. It
            // becomes admissible when any pending datagram releases charge.
            self.wake_blocked_writers();
        }
    }
}
