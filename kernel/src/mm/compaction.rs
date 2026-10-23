//! Explicit bounded compaction, without allocator failure or background policy.
//!
//! Buddy owns the isolated free frames. Migration owns PTE/reference freezing.
//! This coordinator owns only work limits, candidate selection and deferral.
use super::{
    allocator::{
        buddy::compaction::{IsolationKey, RangeError, MAX_COMPACT_ORDER},
        page_frame::{FrameAllocator, PageFrameCount},
    },
    page::migration::{migrate_frame, MigrationError},
    MemoryManagementArch, PhysAddr,
};
use crate::{
    arch::{mm::LockedFrameAllocator, CurrentIrqArch, MMArch},
    exception::{in_interrupt, InterruptArch},
    libs::mutex::Mutex,
    process::ProcessManager,
};

pub(crate) mod recovery;

static EXECUTION: Mutex<()> = Mutex::new(());

pub(crate) const MAX_DEFER_SHIFT: u8 = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Stop {
    Invalid,
    Busy,
    Budget,
    Blocked,
    NoMemory,
    Deferred,
    Timeout,
}
impl From<RangeError> for Stop {
    fn from(error: RangeError) -> Self {
        match error {
            RangeError::Invalid => Self::Invalid,
            RangeError::Busy => Self::Busy,
            RangeError::Budget => Self::Budget,
        }
    }
}

/// An allocated contiguous run, never simultaneously on an ordinary free list.
#[derive(Debug)]
pub(crate) struct CompactedFrames {
    start: PhysAddr,
    pages: usize,
}
impl CompactedFrames {
    pub(crate) fn start(&self) -> PhysAddr {
        self.start
    }
    pub(crate) fn pages(&self) -> usize {
        self.pages
    }

    /// Transfer the run to a caller which will return precisely these frames.
    pub(crate) fn into_raw_parts(mut self) -> (PhysAddr, PageFrameCount) {
        let result = (self.start, PageFrameCount::new(self.pages));
        self.pages = 0;
        result
    }
}
impl Drop for CompactedFrames {
    fn drop(&mut self) {
        if self.pages != 0 {
            unsafe {
                LockedFrameAllocator.free(self.start, PageFrameCount::new(self.pages));
            }
        }
    }
}

/// This lease holds no allocator lock while a source MM is being migrated.
/// Only the final successful capture transfers its free frames to a new owner.
pub(crate) struct Isolation {
    key: Option<IsolationKey>,
    start: PhysAddr,
    pages: usize,
}
impl Isolation {
    pub(crate) fn begin(start: PhysAddr, order: usize, entries: &mut usize) -> Result<Self, Stop> {
        let key =
            LockedFrameAllocator::with_buddy(|buddy| buddy.begin_isolation(start, order, entries))
                .ok_or(Stop::NoMemory)??;
        Ok(Self {
            key: Some(key),
            start,
            pages: 1 << order,
        })
    }
    pub(crate) fn is_free(&self, address: PhysAddr) -> bool {
        self.key.as_ref().is_some_and(|key| {
            LockedFrameAllocator::with_buddy(|buddy| buddy.isolated_free(key, address))
                .unwrap_or(false)
        })
    }
    pub(crate) fn free_pages(&self) -> usize {
        self.key.as_ref().map_or(0, |key| {
            LockedFrameAllocator::with_buddy(|buddy| buddy.isolated_free_pages(key)).unwrap_or(0)
        })
    }
    pub(crate) fn capture(&mut self) -> Option<CompactedFrames> {
        let key = self.key.as_ref()?;
        let start = LockedFrameAllocator::with_buddy(|buddy| buddy.capture_isolation(key))??;
        assert_eq!(start, self.start);
        self.key = None;
        Some(CompactedFrames {
            start,
            pages: self.pages,
        })
    }
}
impl Drop for Isolation {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            LockedFrameAllocator::with_buddy(|buddy| buddy.release_isolation(&key))
                .expect("compaction allocator disappeared");
        }
    }
}

/// Ordinary allocation under the same physical domain as the compaction request.
/// Exhausting a scan budget is distinct from finding no suitable free block.
pub(crate) fn try_allocate_in_range(
    start: PhysAddr,
    end: PhysAddr,
    order: usize,
    entries: &mut usize,
) -> Result<Option<CompactedFrames>, Stop> {
    LockedFrameAllocator::with_buddy(|buddy| buddy.allocate_in_range(order, start, end, entries))
        .ok_or(Stop::NoMemory)?
        .map(|found| {
            found.map(|start| CompactedFrames {
                start,
                pages: 1 << order,
            })
        })
        .map_err(Stop::from)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub targets: usize,
    pub entries: usize,
    pub pages: usize,
    pub migrations: usize,
    pub rmaps: usize,
    pub no_progress: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            targets: 8,
            entries: 16384,
            pages: 2048,
            migrations: 1024,
            rmaps: 64,
            no_progress: 2,
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Request {
    pub start: PhysAddr,
    pub end: PhysAddr,
    pub order: usize,
    /// Optional first scan address within this stable domain. Changing only
    /// the seed preserves the domain/order failure deferral history.
    pub start_at: Option<PhysAddr>,
    /// Cooperative deadline; an in-flight migration/TLB operation completes first.
    pub deadline: Option<u64>,
    pub limits: Limits,
}
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Stats {
    pub targets: usize,
    pub entries: usize,
    pub pages: usize,
    pub migrations: usize,
    pub moved: usize,
}
#[derive(Debug)]
pub(crate) struct Attempt {
    pub outcome: Result<CompactedFrames, Stop>,
    pub stats: Stats,
    pub next: PhysAddr,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Domain {
    start: PhysAddr,
    end: PhysAddr,
    order: usize,
}

/// State belongs to a requesting service, which decides when to call again.
/// No worker, timer, or globally retained per-region allocation is introduced.
pub(crate) struct Compactor {
    domain: Option<Domain>,
    defer_shift: u8,
    defer_left: u8,
}
impl Compactor {
    pub(crate) const fn new() -> Self {
        Self {
            domain: None,
            defer_shift: 0,
            defer_left: 0,
        }
    }
    pub(crate) fn allocate(&mut self, request: Request) -> Attempt {
        let mut stats = Stats::default();
        let mut next = request.start;
        let result = self.execute(request, &mut stats, &mut next);
        if result.is_ok() {
            self.defer_shift = 0;
            self.defer_left = 0;
        } else if stats.moved == 0 && matches!(result, Err(Stop::Blocked | Stop::NoMemory)) {
            self.defer_shift = (self.defer_shift + 1).min(MAX_DEFER_SHIFT);
            self.defer_left = (1u8 << self.defer_shift) - 1;
        }
        Attempt {
            outcome: result,
            stats,
            next,
        }
    }
    fn execute(
        &mut self,
        request: Request,
        stats: &mut Stats,
        next: &mut PhysAddr,
    ) -> Result<CompactedFrames, Stop> {
        if !CurrentIrqArch::is_irq_enabled()
            || in_interrupt()
            || ProcessManager::current_pcb().preempt_count() != 0
        {
            return Err(Stop::Busy);
        }
        let Request {
            start,
            end,
            order,
            start_at,
            deadline,
            limits,
        } = request;
        if order == 0
            || order > MAX_COMPACT_ORDER
            || start.data() & MMArch::PAGE_OFFSET_MASK != 0
            || end.data() & MMArch::PAGE_OFFSET_MASK != 0
            || start >= end
            || limits.targets == 0
            || limits.targets > 64
            || limits.entries > 1 << 20
            || limits.pages > 4096
            || limits.migrations > 4096
            || limits.rmaps == 0
            || limits.rmaps > 128
            || limits.no_progress == 0
            || limits.no_progress > 64
        {
            return Err(Stop::Invalid);
        }
        let scan_start = start_at.unwrap_or(start);
        if scan_start < start
            || scan_start >= end
            || scan_start.data() & MMArch::PAGE_OFFSET_MASK != 0
        {
            return Err(Stop::Invalid);
        }
        let bytes = MMArch::PAGE_SIZE << order;
        let mut candidate = scan_start
            .data()
            .checked_add(bytes - 1)
            .map(|address| address & !(bytes - 1))
            .ok_or(Stop::Invalid)?;
        if candidate
            .checked_add(bytes)
            .is_none_or(|last| last > end.data())
        {
            return Err(Stop::Invalid);
        }
        *next = PhysAddr::new(candidate);
        let expired = || deadline.is_some_and(|until| crate::time::timer::clock() >= until);
        if expired() {
            return Err(Stop::Timeout);
        }
        let domain = Domain { start, end, order };
        if self.domain != Some(domain) {
            self.domain = Some(domain);
            self.defer_shift = 0;
            self.defer_left = 0;
        }
        let _execution = EXECUTION.try_lock().map_err(|_| Stop::Busy)?;
        let mut remaining = limits.entries;
        let fast = try_allocate_in_range(start, end, order, &mut remaining);
        stats.entries = limits.entries - remaining;
        if let Some(run) = fast? {
            return Ok(run);
        }
        // Ordinary allocation is always attempted before applying deferral.
        if self.defer_left != 0 {
            self.defer_left -= 1;
            return Err(Stop::Deferred);
        }
        let mut no_progress = 0;
        let mut saw_busy = false;
        while candidate
            .checked_add(bytes)
            .is_some_and(|last| last <= end.data())
        {
            if expired() {
                return Err(Stop::Timeout);
            }
            if stats.targets == limits.targets {
                return Err(if saw_busy { Stop::Busy } else { Stop::Budget });
            }
            stats.targets += 1;
            let lease = Isolation::begin(PhysAddr::new(candidate), order, &mut remaining);
            stats.entries = limits.entries - remaining;
            let mut lease = lease?;
            let before = lease.free_pages();
            let mut stopped = Stop::Blocked;
            for offset in 0..(1usize << order) {
                if expired() {
                    return Err(Stop::Timeout);
                }
                if lease.free_pages() == 1 << order {
                    break;
                }
                if stats.pages == limits.pages {
                    return Err(Stop::Budget);
                }
                stats.pages += 1;
                let physical = PhysAddr::new(candidate + offset * MMArch::PAGE_SIZE);
                if lease.is_free(physical) {
                    continue;
                }
                if stats.migrations == limits.migrations {
                    return Err(Stop::Budget);
                }
                stats.migrations += 1;
                match migrate_frame(physical, limits.rmaps) {
                    Ok(_) if lease.is_free(physical) => stats.moved += 1,
                    Err(MigrationError::NoMemory) => {
                        stopped = Stop::NoMemory;
                        break;
                    }
                    Err(MigrationError::Busy) => {
                        saw_busy = true;
                        break;
                    }
                    _ => break,
                }
            }
            if let Some(run) = lease.capture() {
                return Ok(run);
            }
            if lease.free_pages() == before {
                no_progress += 1;
            } else {
                no_progress = 0;
            }
            drop(lease);
            *next = PhysAddr::new(candidate + bytes);
            if stopped == Stop::NoMemory {
                return Err(stopped);
            }
            if no_progress >= limits.no_progress {
                return Err(if saw_busy { Stop::Busy } else { Stop::Blocked });
            }
            candidate += bytes;
            *next = PhysAddr::new(candidate);
        }
        Err(if saw_busy { Stop::Busy } else { Stop::Blocked })
    }
}
