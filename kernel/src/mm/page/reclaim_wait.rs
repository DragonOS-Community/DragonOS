//! Reusable waits for the one permanent background reclaimer.
//!
//! The ordinary tick supplies deadlines; no Timer or waiter is allocated by
//! a pressure pass. Only the memory notifier wakes the pinned infrastructure
//! task, outside arbitrary page/filesystem resource locks.
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::{
    filesystem::page_cache::{async_writeback_in_flight, async_writeback_progress_snapshot},
    libs::{
        spinlock::SpinLock,
        wait_queue::{Waiter, Waker},
    },
    time::timer::{clock, next_n_us_timer_jiffies},
};

static DEADLINE: AtomicU64 = AtomicU64::new(0);
static PRESSURE_EPOCH: AtomicU64 = AtomicU64::new(0);

pub(crate) fn pressure_epoch() -> u64 {
    PRESSURE_EPOCH.load(Ordering::Acquire)
}

pub(crate) fn request_pressure() {
    PRESSURE_EPOCH.fetch_add(1, Ordering::Release);
    crate::mm::memory_notify::request_reclaim_deadline();
}
static WRITEBACK_WAITING: AtomicBool = AtomicBool::new(false);
// Never removed after bootstrap. The permanent reclaimer PCB is also pinned
// by its creator/PAGE_RECLAIMER_THREAD, so notifier clones cannot retire it.
static WAKER: SpinLock<Option<Arc<Waker>>> = SpinLock::new(None);

pub(super) struct ReclaimWait {
    waiter: Waiter,
}

struct ArmedWait;

impl Drop for ArmedWait {
    fn drop(&mut self) {
        WRITEBACK_WAITING.store(false, Ordering::Release);
        DEADLINE.store(0, Ordering::Release);
    }
}

impl ReclaimWait {
    /// Called once at thread entry, before any scan or deadline publication.
    pub(super) fn new() -> Self {
        let (waiter, waker) =
            Waiter::try_new_pair().expect("background reclaimer waiter bootstrap failed");
        let mut slot = WAKER.lock_irqsave();
        assert!(
            slot.is_none(),
            "background reclaimer waiter initialized twice"
        );
        *slot = Some(waker);
        Self { waiter }
    }

    fn arm(&self, microseconds: u64, writeback: bool) -> (ArmedWait, u64) {
        let deadline = next_n_us_timer_jiffies(microseconds).max(1);
        // Only this task arms/clears this slot. An old notification may wake a
        // new wait, but it cannot clear or complete the newly armed deadline.
        DEADLINE.store(deadline, Ordering::Release);
        WRITEBACK_WAITING.store(writeback, Ordering::Release);
        (ArmedWait, deadline)
    }

    pub(super) fn pause_us(&self, microseconds: u64, observed_epoch: u64) {
        let (_armed, deadline) = self.arm(microseconds, false);
        while clock() < deadline && pressure_epoch() == observed_epoch {
            self.waiter
                .wait(false)
                .expect("background reclaimer pause failed");
        }
    }

    /// False preserves the caller's short backoff when there was no writeback
    /// to wait for. A deadline limits waiting even if an I/O backend is stuck.
    pub(super) fn wait_for_writeback_progress(&self, observed: u64) -> bool {
        if async_writeback_progress_snapshot() != observed || !async_writeback_in_flight() {
            return false;
        }
        let (_armed, deadline) = self.arm(10_000, true);
        let mut waited = false;
        while clock() < deadline
            && async_writeback_progress_snapshot() == observed
            && async_writeback_in_flight()
        {
            waited = true;
            self.waiter
                .wait(false)
                .expect("background writeback wait failed");
        }
        waited
    }
}

/// Called after the common jiffy counter advances, on its existing tick owner.
/// Never consumes DEADLINE: a stale tick must not erase the task's new arm.
pub(crate) fn tick(now: u64) {
    let deadline = DEADLINE.load(Ordering::Acquire);
    if deadline != 0 && now >= deadline {
        crate::mm::memory_notify::request_reclaim_deadline();
    }
}

/// Called only after publishing the real writeback completion generation.
pub(crate) fn request_writeback_progress() {
    if WRITEBACK_WAITING.load(Ordering::Acquire) {
        crate::mm::memory_notify::request_reclaim_deadline();
    }
}

/// Memory-notifier task context only; the slot guard is gone before ttwu.
pub(crate) fn notify_deadline() {
    let waker = WAKER.lock_irqsave().as_ref().cloned();
    if let Some(waker) = waker {
        waker.wake();
    }
}
