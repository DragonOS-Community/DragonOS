use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
pub(crate) struct ShutdownRecvTracker {
    limit: AtomicUsize,
    read: AtomicUsize,
}

impl ShutdownRecvTracker {
    pub(crate) const fn new() -> Self {
        Self {
            limit: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
        }
    }

    #[inline]
    pub(crate) fn init(&self, queued: usize) {
        self.limit.store(queued, Ordering::Relaxed);
        self.read.store(0, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    #[inline]
    pub(crate) fn remaining_limit(&self) -> usize {
        let limit = self.limit.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Relaxed);
        limit.saturating_sub(read)
    }

    #[inline]
    pub(crate) fn record_read(&self, n: usize) -> bool {
        let limit = self.limit.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Relaxed);
        let new_read = (read + n).min(limit);
        self.read.store(new_read, Ordering::Relaxed);
        new_read >= limit
    }
}

impl Default for ShutdownRecvTracker {
    fn default() -> Self {
        Self::new()
    }
}
