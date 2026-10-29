//! Physical allocation recovery at the resource-lock-free user fault boundary.
//! Charge/memcg failures retain their separate policy and limiting account.

use core::sync::atomic::{AtomicUsize, Ordering};
use system_error::SystemError;

use crate::{
    arch::{ipc::signal::Signal, mm::LockedFrameAllocator},
    filesystem::page_cache::{async_writeback_in_flight, async_writeback_progress_snapshot},
    libs::resource_wait::wait_until_killable,
    mm::{
        allocator::page_frame::PageFrameCount,
        page::{PageReclaimer, ReclaimProgress},
    },
    process::{ProcessFlags, ProcessManager},
    time::timer::{clock, next_n_us_timer_jiffies},
};

const MAX_NO_PROGRESS: u8 = 16;
static WAITERS: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct GlobalFaultRecoveryState {
    remaining: u8,
    no_release_deadline: Option<u64>,
    round_remaining: Option<usize>,
}

impl GlobalFaultRecoveryState {
    pub(crate) const fn new() -> Self {
        Self {
            remaining: MAX_NO_PROGRESS,
            no_release_deadline: None,
            round_remaining: None,
        }
    }

    /// Freeze the inventory at the first successful slice of each round.
    /// Concurrent scanners rotate the LRU too: this is bounded approximate
    /// coverage, not a claim that every individual PFN was visited once.
    fn record_scan(&mut self, progress: Option<ReclaimProgress>) -> bool {
        let Some(progress) = progress else {
            return false;
        };
        if progress.reclaimed != 0 {
            self.remaining = MAX_NO_PROGRESS;
            self.no_release_deadline = None;
            self.round_remaining = None;
            return true;
        }
        if progress.visited == 0 {
            // Successful empty/ineligible scans must not spin forever.
            self.round_remaining = None;
            self.remaining = self.remaining.saturating_sub(1);
        } else {
            let remaining = self.round_remaining.get_or_insert(progress.inventory);
            *remaining = remaining.saturating_sub(progress.visited);
            if *remaining == 0 {
                self.round_remaining = None;
                self.remaining = self.remaining.saturating_sub(1);
            }
        }
        false
    }

    fn expired(&self, now: u64) -> bool {
        self.no_release_deadline
            .is_some_and(|deadline| now >= deadline)
    }

    /// True requests a real new fault attempt; false hands exhaustion to the
    /// existing OOM coordinator. No candidate, MM/resource or IRQ guard may
    /// survive into this method. Reserve access ends inside the scan itself.
    pub(crate) fn retry(&mut self) -> Result<bool, SystemError> {
        if interrupted() {
            return Err(SystemError::EINTR);
        }
        self.no_release_deadline
            .get_or_insert_with(|| next_n_us_timer_jiffies(10_000_000));
        // This bound is independent of inventory, Busy ownership, I/O
        // completions and scan-round counters. Only physical release resets it.
        if self.expired(clock()) {
            return Ok(false);
        }
        let observed = async_writeback_progress_snapshot();
        let pressure = LockedFrameAllocator::pressure_snapshot();
        let zones = (pressure.total_deficit() != 0).then_some(pressure);
        let progress = PageReclaimer::try_shrink_global(PageFrameCount::new(64), 4096, zones);
        if self.record_scan(progress) {
            return Ok(true);
        }
        let in_flight = async_writeback_in_flight();
        // The completed-round count is not an I/O latency bound. Real in-flight
        // writeback may extend it within ten seconds without a release. Neither
        // unrelated completions nor repeated scan contention renew this window.
        // Recheck after scanning too: existing resource-lock waits may have
        // consumed the time window. A completion alone never resets it.
        if self.expired(clock()) || (self.remaining == 0 && !in_flight) {
            return Ok(false);
        }
        // Busy is different from an empty scan. Wait for its owner without
        // taking the global scan lock; every wait has a tick-driven deadline.
        if progress.is_none() {
            wait_for_progress(observed, false, 10_000)?;
        } else if in_flight {
            wait_for_progress(observed, true, 100_000)?;
        } else {
            crate::sched::cond_resched();
        }
        // Completion is permission to retry, not evidence that physical
        // pages were freed. In particular it never renews the retry budget.
        Ok(true)
    }
}

#[cfg(feature = "mm_selftest")]
pub(crate) fn budget_selftest() -> bool {
    let mut state = GlobalFaultRecoveryState::new();
    state.no_release_deadline = Some(100);
    state.record_scan(None);
    if state.remaining != MAX_NO_PROGRESS || state.round_remaining.is_some() {
        return false;
    }
    let slice = |inventory| ReclaimProgress {
        visited: 4096,
        inventory,
        ..Default::default()
    };
    state.record_scan(Some(slice(10_000)));
    state.record_scan(None);
    if state.round_remaining != Some(5904) || state.remaining != MAX_NO_PROGRESS {
        return false;
    }
    // A larger later sample cannot move the already established round target.
    state.record_scan(Some(slice(usize::MAX)));
    if state.remaining != MAX_NO_PROGRESS || state.round_remaining != Some(1808) {
        return false;
    }
    state.record_scan(Some(slice(usize::MAX)));
    if state.remaining != MAX_NO_PROGRESS - 1 || state.round_remaining.is_some() {
        return false;
    }
    state.record_scan(Some(ReclaimProgress::default()));
    if state.remaining != MAX_NO_PROGRESS - 2 || !state.expired(100) || state.expired(99) {
        return false;
    }
    state.record_scan(Some(ReclaimProgress {
        reclaimed: 1,
        ..Default::default()
    }));
    state.remaining == MAX_NO_PROGRESS
        && state.no_release_deadline.is_none()
        && state.round_remaining.is_none()
}

fn interrupted() -> bool {
    let task = ProcessManager::current_pcb();
    Signal::oom_fatal_signal_pending(&task) || task.flags().contains(ProcessFlags::EXITING)
}

fn wait_for_progress(observed: u64, writeback: bool, timeout_us: u64) -> Result<(), SystemError> {
    struct Registration;
    impl Drop for Registration {
        fn drop(&mut self) {
            WAITERS.fetch_sub(1, Ordering::AcqRel);
        }
    }
    let deadline = next_n_us_timer_jiffies(timeout_us);
    WAITERS.fetch_add(1, Ordering::AcqRel);
    let _registration = Registration;
    // The embedded ResourceWaitLink registers before rechecking the real
    // predicate. No Waiter, timer object or heap allocation is needed here.
    wait_until_killable(|| {
        (clock() >= deadline
            || async_writeback_progress_snapshot() != observed
            || (writeback && !async_writeback_in_flight()))
        .then_some(())
    })
}

/// The common tick only queues the existing memory-notifier worker. It never
/// wakes arbitrary tasks under the timer's locks or borrows reserve access.
pub(crate) fn tick() {
    if WAITERS.load(Ordering::Acquire) != 0 {
        super::memory_notify::request_oom_progress();
    }
}
