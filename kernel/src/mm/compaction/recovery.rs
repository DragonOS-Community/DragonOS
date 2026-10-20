//! Opt-in recovery for a proven fragmented contiguous DMA request.
//!
//! The caller supplies the allocation evidence and a sleepable lock context.
//! This owner supplies only bounded candidate discovery and persistent backoff;
//! it never changes the ordinary heap/frame allocator's failure policy.
use super::{CompactedFrames, Compactor, Limits, Request, Stop, MAX_COMPACT_ORDER};
use crate::{
    arch::{CurrentIrqArch, MMArch},
    cgroup::memory::io::noio_active,
    exception::{in_interrupt, InterruptArch},
    libs::mutex::Mutex,
    mm::{
        allocator::buddy::DmaRecoveryZone,
        memblock::{mem_block_manager, MemoryAreaAttr},
        page::compaction_candidates,
        MemoryManagementArch, PhysAddr,
    },
    process::ProcessManager,
};

const DMA32_END: usize = 1usize << 32;

struct Service {
    compactor: Compactor,
    cursor: usize,
}
impl Service {
    const fn new() -> Self {
        Self {
            compactor: Compactor::new(),
            cursor: 0,
        }
    }
}

// A fixed order-indexed owner, not an unbounded map keyed by device or mask.
// Try admission also prevents recursive DMA recovery while preparing metadata.
static RECOVERY: Mutex<[Service; MAX_COMPACT_ORDER]> =
    Mutex::new([const { Service::new() }; MAX_COMPACT_ORDER]);

/// This is an additional runtime guard, not permission to enter while holding
/// driver, pool, MM, Page, registry or filesystem I/O resource locks.
fn context_allowed() -> bool {
    ProcessManager::initialized()
        && CurrentIrqArch::is_irq_enabled()
        && !in_interrupt()
        && ProcessManager::current_pcb().preempt_count() == 0
        && !noio_active()
}

/// Recover only after the same DMA attempt established fragmentation.
/// `max` is the inclusive device address limit; usize::MAX means unrestricted.
/// `zone` carries the same attempt's capacity proof and reserve policy.
/// A captured run is transferred directly to the DMA owner, never freed first.
pub(crate) fn allocate(
    order: usize,
    max: PhysAddr,
    zone: DmaRecoveryZone,
) -> Result<CompactedFrames, Stop> {
    if order == 0 || order > MAX_COMPACT_ORDER {
        return Err(Stop::Invalid);
    }
    if !context_allowed() {
        return Err(Stop::Busy);
    }
    let mut services = RECOVERY.try_lock().map_err(|_| Stop::Busy)?;
    let service = &mut services[order - 1];
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
        return Err(Stop::Invalid);
    }
    let candidates =
        compaction_candidates(&mut service.cursor, minimum, maximum).map_err(|_| Stop::Busy)?;
    for physical in candidates.iter().flatten() {
        if let Some(request) = candidate_request(*physical, order, max, normal) {
            // One candidate per failed allocation. Changing only its seed
            // leaves this domain/order's Compactor deferral intact.
            return service.compactor.allocate(request).outcome;
        }
    }
    Err(Stop::Blocked)
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
    // before any allocator preparation, isolation, migration or TLB operation.
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
        limits: Limits {
            targets: 1,
            entries: 16384,
            pages: 512,
            migrations: 512,
            rmaps: 64,
            no_progress: 1,
        },
    })
}
