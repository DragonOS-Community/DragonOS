//! Opt-in recovery for a proven fragmented contiguous DMA request.
//!
//! The caller supplies the allocation evidence and a sleepable lock context.
//! This owner supplies only bounded candidate discovery and persistent backoff;
//! it never changes the ordinary heap/frame allocator's failure policy.
use super::{CompactedFrames, Compactor, Limits, Request, Stop, MAX_COMPACT_ORDER};
use crate::{
    arch::{mm::LockedFrameAllocator, CurrentIrqArch, MMArch},
    cgroup::memory::io::noio_active,
    exception::{in_interrupt, InterruptArch},
    libs::mutex::Mutex,
    mm::{
        allocator::buddy::DmaRecoveryZone,
        memblock::{mem_block_manager, MemoryAreaAttr},
        page::compaction_candidate,
        MemoryManagementArch, PhysAddr,
    },
    process::ProcessManager,
};

const DMA32_END: usize = 1usize << 32;

/// Discovery has a bounded registry window distinct from target execution.
/// Crossing its table boundary remains an unsuccessful round; advancing within
/// a window is not evidence that compaction itself has failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ServiceError {
    DiscoveryProgress,
    Stopped(Stop),
}
impl From<Stop> for ServiceError {
    fn from(stop: Stop) -> Self {
        Self::Stopped(stop)
    }
}
impl ServiceError {
    fn discovery_miss(before: usize, after: usize) -> Self {
        if after > before {
            Self::DiscoveryProgress
        } else {
            // This is a scanner boundary, not proof that a whole scan used the
            // same mask: wide/narrow requests deliberately share this cursor.
            Self::Stopped(Stop::Blocked)
        }
    }

    fn into_stop(self) -> Stop {
        match self {
            Self::DiscoveryProgress => Stop::Blocked,
            Self::Stopped(stop) => stop,
        }
    }
}

pub(super) struct Service {
    compactor: Compactor,
    cursor: usize,
    #[cfg(feature = "mm_selftest")]
    pub(super) last_moved: usize,
}
impl Service {
    pub(super) const fn new() -> Self {
        Self {
            compactor: Compactor::new(),
            cursor: 0,
            #[cfg(feature = "mm_selftest")]
            last_moved: 0,
        }
    }
}

// A fixed order-indexed owner, not an unbounded map keyed by device or mask.
// Try admission also prevents recursive DMA recovery during migration allocations.
static RECOVERY: Mutex<[Service; MAX_COMPACT_ORDER]> =
    Mutex::new([const { Service::new() }; MAX_COMPACT_ORDER]);

/// This is an additional runtime guard, not permission to enter while holding
/// driver, pool, MM, Page, registry or filesystem I/O resource locks.
pub(crate) fn context_allowed() -> bool {
    ProcessManager::initialized()
        && CurrentIrqArch::is_irq_enabled()
        && !in_interrupt()
        && ProcessManager::current_pcb().preempt_count() == 0
        && !noio_active()
        && crate::mm::allocator::reserve::kernel_level()
            == crate::mm::allocator::reserve::ReserveLevel::Kernel
}

/// Recover only after the same DMA attempt established fragmentation.
/// `max` is the inclusive device address limit; usize::MAX means unrestricted.
/// `zone` carries the same attempt's capacity proof and reserve policy.
/// A captured run is transferred directly to the DMA owner, never freed first.
pub(crate) fn allocate(
    order: usize,
    max: PhysAddr,
    zone: DmaRecoveryZone,
    deadline: u64,
) -> Result<CompactedFrames, Stop> {
    if order == 0 || order > MAX_COMPACT_ORDER {
        return Err(Stop::Invalid);
    }
    if !context_allowed() {
        return Err(Stop::Busy);
    }
    let mut services = RECOVERY.try_lock().map_err(|_| Stop::Busy)?;
    let service = &mut services[order - 1];
    service
        .allocate(order, max, zone, Some(deadline))
        .map_err(ServiceError::into_stop)
}

impl Service {
    pub(super) fn allocate(
        &mut self,
        order: usize,
        max: PhysAddr,
        zone: DmaRecoveryZone,
        deadline: Option<u64>,
    ) -> Result<CompactedFrames, ServiceError> {
        #[cfg(feature = "mm_selftest")]
        {
            self.last_moved = 0;
        }
        if deadline.is_some_and(|until| crate::time::timer::clock() >= until) {
            return Err(Stop::Timeout.into());
        }
        // The same failed buddy attempt proved capacity in this one zone.
        // Do not spend its evidence on a different, capacity-starved domain.
        let normal = matches!(zone, DmaRecoveryZone::Normal);
        let minimum = PhysAddr::new(if normal { DMA32_END } else { 0 });
        let maximum = if normal {
            max
        } else {
            PhysAddr::new(max.data().min(u32::MAX as usize))
        };
        if minimum > maximum {
            return Err(Stop::Invalid.into());
        }
        let cursor_before = self.cursor;
        let physical =
            compaction_candidate(&mut self.cursor, minimum, maximum).map_err(|_| Stop::Busy)?;
        let Some(physical) = physical else {
            return Err(ServiceError::discovery_miss(cursor_before, self.cursor));
        };
        if let Some(mut request) = candidate_request(physical, order, max, normal) {
            // Deferred work did not consume this candidate. Keep its discovery
            // position so the next real attempt cannot skip it in lockstep with
            // the domain's exponential backoff.
            request.deadline = deadline;
            let attempt = self.compactor.allocate(request);
            #[cfg(feature = "mm_selftest")]
            {
                self.last_moved = attempt.stats.moved;
            }
            let outcome = attempt.outcome;
            if matches!(&outcome, Err(Stop::Deferred)) {
                self.cursor = cursor_before;
            }
            let run = outcome?;
            let level = crate::mm::allocator::reserve::kernel_level();
            if !LockedFrameAllocator::with_buddy(|buddy| {
                buddy.held_handoff_allowed(run.start(), level)
                    && (!matches!(zone, DmaRecoveryZone::Dma32Fallback)
                        || buddy.dma32_fallback_allowed(0))
            })
            .unwrap_or(false)
            {
                drop(run);
                return Err(Stop::Busy.into());
            }
            return Ok(run);
        }
        Err(Stop::Blocked.into())
    }
}

fn candidate_request(
    physical: PhysAddr,
    order: usize,
    max: PhysAddr,
    normal: bool,
) -> Option<Request> {
    let bytes = MMArch::PAGE_SIZE << order;
    let seed = physical.data() & !(bytes - 1);
    let candidate_end = seed.checked_add(bytes)?;
    if (seed >= DMA32_END) != normal || candidate_end.checked_sub(1)? > max.data() {
        return None;
    }
    // Only copy a physical RAM descriptor. The memblock spin guard is gone
    // before any isolation, migration or TLB operation.
    let area = {
        let mut regions = mem_block_manager().to_iter();
        regions.find(|area| {
            !area.flags.intersects(
                MemoryAreaAttr::RESERVED | MemoryAreaAttr::NOMAP | MemoryAreaAttr::DRIVER_MANAGED,
            ) && area.base.data() <= seed
                && area
                    .base
                    .data()
                    .checked_add(area.size)
                    .is_some_and(|end| candidate_end <= end)
        })
    }?;
    let zone_start = if normal { DMA32_END } else { 0 };
    let zone_end = if normal { usize::MAX } else { DMA32_END };
    let mask_end = max.data().saturating_add(1);
    let start = area
        .base
        .data()
        .max(zone_start)
        .checked_add(MMArch::PAGE_OFFSET_MASK)?
        & !MMArch::PAGE_OFFSET_MASK;
    let end = area
        .base
        .data()
        .checked_add(area.size)?
        .min(zone_end)
        .min(mask_end)
        & !MMArch::PAGE_OFFSET_MASK;
    if seed < start || candidate_end > end {
        return None;
    }
    Some(Request {
        start: PhysAddr::new(start),
        end: PhysAddr::new(end),
        order,
        start_at: Some(PhysAddr::new(seed)),
        deadline: None,
        limits: Limits {
            targets: 8,
            entries: 16384,
            pages: 512,
            migrations: 512,
            rmaps: 64,
            no_progress: 8,
        },
    })
}

/// Formal contention coverage owns only the existing synchronous service lock.
/// The independent background service remains runnable throughout the request.
#[cfg(feature = "mm_selftest")]
pub(crate) mod selftest {
    #[inline(never)]
    pub(crate) fn discovery_boundaries(buckets: usize) -> bool {
        use super::{
            DmaRecoveryZone, MMArch, MemoryManagementArch, PhysAddr, Service, ServiceError, Stop,
        };
        if buckets == 0 {
            return false;
        }
        let mut service = Service::new();
        // Physical page zero is reserved by boot and checked by the caller.
        // No candidate override is used: both queries inspect the real registry.
        let first = service.allocate(
            1,
            PhysAddr::new(MMArch::PAGE_SIZE - 1),
            DmaRecoveryZone::Dma32,
            None,
        );
        let advancing = if buckets > 256 {
            matches!(first, Err(ServiceError::DiscoveryProgress)) && service.cursor == 256
        } else {
            matches!(first, Err(ServiceError::Stopped(Stop::Blocked))) && service.cursor == 0
        };
        service.cursor = buckets - 1;
        let boundary = service.allocate(
            1,
            PhysAddr::new(MMArch::PAGE_SIZE - 1),
            DmaRecoveryZone::Dma32,
            None,
        );
        advancing
            && matches!(boundary, Err(ServiceError::Stopped(Stop::Blocked)))
            && ServiceError::discovery_miss(512, 256) == ServiceError::Stopped(Stop::Blocked)
            && ServiceError::DiscoveryProgress.into_stop() == Stop::Blocked
    }

    pub(crate) fn allocate_with_direct_contention(
        pages: usize,
        options: crate::mm::dma::DmaAllocOptions,
    ) -> Result<crate::mm::dma::DmaBuffer, system_error::SystemError> {
        let _direct = super::RECOVERY
            .try_lock()
            .map_err(|_| system_error::SystemError::EBUSY)?;
        crate::mm::dma::DmaBuffer::try_alloc_pages(pages, options)
    }
}

/// Caller owns a fragmented prefix whose seeded target begins with a raw
/// blocker. This local owner never resets or seeds the production RECOVERY.
#[cfg(feature = "mm_selftest")]
pub(crate) fn deferred_discovery_selftest(
    physical: PhysAddr,
    cursor: usize,
    order: usize,
    max: PhysAddr,
) -> bool {
    let Some(mut request) = candidate_request(physical, order, max, false) else {
        return false;
    };
    request.limits.targets = 1;
    request.limits.no_progress = 1;
    let mut service = Service::new();
    // Establish backoff with the real blocked target, not a synthetic flag.
    if !matches!(
        service.compactor.allocate(request).outcome,
        Err(Stop::Blocked)
    ) {
        return false;
    }
    service.cursor = cursor;
    let mut expected = cursor;
    if !matches!(compaction_candidate(&mut expected, PhysAddr::new(0), max), Ok(Some(found)) if found == physical)
    {
        return false;
    }
    let deferred = service.allocate(order, max, DmaRecoveryZone::Dma32, None);
    if !matches!(deferred, Err(ServiceError::Stopped(Stop::Deferred))) || service.cursor != cursor {
        return false;
    }
    let attempted = service.allocate(order, max, DmaRecoveryZone::Dma32, None);
    !matches!(
        attempted,
        Err(ServiceError::Stopped(
            Stop::Deferred | Stop::Invalid | Stop::Timeout
        ))
    ) && service.cursor == expected
}
