use core::sync::atomic::{AtomicUsize, Ordering};

const CLOSED: usize = 1usize << (usize::BITS - 1);
const ACTIVE_MASK: usize = !CLOSED;

/// Serializes the final device-TX admission point with administrative DOWN.
///
/// Callers acquire a guard only around the synchronous raw-TX path; user
/// copies and packet construction must happen before admission. Closing the
/// gate prevents new submissions and waits for already admitted ones.
pub(super) struct TxAdmission {
    state: AtomicUsize,
}

pub(crate) struct TxAdmissionGuard<'a> {
    admission: &'a TxAdmission,
}

impl TxAdmission {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            state: AtomicUsize::new(if enabled { 0 } else { CLOSED }),
        }
    }

    pub(super) fn try_enter(&self) -> Option<TxAdmissionGuard<'_>> {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & CLOSED != 0 || state & ACTIVE_MASK == ACTIVE_MASK {
                return None;
            }
            match self.state.compare_exchange_weak(
                state,
                state + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(TxAdmissionGuard { admission: self }),
                Err(current) => state = current,
            }
        }
    }

    pub(super) fn close_and_wait(&self) {
        let previous = self.state.fetch_or(CLOSED, Ordering::AcqRel);
        debug_assert_eq!(previous & CLOSED, 0);
        while self.state.load(Ordering::Acquire) != CLOSED {
            crate::sched::sched_yield();
        }
    }

    pub(super) fn open(&self) {
        debug_assert_eq!(self.state.load(Ordering::Acquire), CLOSED);
        self.state.store(0, Ordering::Release);
    }
}

impl Drop for TxAdmissionGuard<'_> {
    fn drop(&mut self) {
        let previous = self.admission.state.fetch_sub(1, Ordering::Release);
        debug_assert_ne!(previous & ACTIVE_MASK, 0);
    }
}
