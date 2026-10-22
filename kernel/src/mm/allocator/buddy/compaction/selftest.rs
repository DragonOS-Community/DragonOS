//! Private allocator structure tests over an exclusively owned RAM backing.
//! Real Page migration is tested separately against the global allocator.
use super::*;
use crate::{
    arch::{mm::LockedFrameAllocator, MMArch},
    mm::allocator::page_frame::{FrameAllocator, PageFrameCount},
};
use alloc::{vec, vec::Vec};
use core::marker::PhantomData;

const BACKING_PAGES: usize = 2048;
const TARGET_OFFSET: usize = 1024;
const TARGET_PAGES: usize = 512;

struct Backing(PhysAddr);
impl Drop for Backing {
    fn drop(&mut self) {
        unsafe { LockedFrameAllocator.free(self.0, PageFrameCount::new(BACKING_PAGES)) };
    }
}

unsafe fn empty(backing: PhysAddr) -> BuddyAllocator<MMArch> {
    let mut dma32_empty_area = [PhysAddr::new(0); super::super::BUDDY_ORDER_COUNT];
    let mut normal_empty_area = [PhysAddr::new(0); super::super::BUDDY_ORDER_COUNT];
    for (index, area) in dma32_empty_area
        .iter_mut()
        .chain(normal_empty_area.iter_mut())
        .enumerate()
    {
        *area = backing + (index << MIN_ORDER);
        BuddyAllocator::<MMArch>::write_page(*area, PageList::new(0, PhysAddr::new(0)));
    }
    // Two order-0 metadata pages avoid turning a target free page into metadata
    // when the multi-page discovery test exceeds one page's 510 entries.
    let extra = backing + (40 << MIN_ORDER);
    let dma_first = dma32_empty_area[0];
    BuddyAllocator::<MMArch>::write_page(extra, PageList::new(0, dma_first));
    dma32_empty_area[0] = extra;
    let extra = backing + (41 << MIN_ORDER);
    let normal_first = normal_empty_area[0];
    BuddyAllocator::<MMArch>::write_page(extra, PageList::new(0, normal_first));
    normal_empty_area[0] = extra;
    BuddyAllocator {
        dma32_free_area: [PhysAddr::new(0); super::super::BUDDY_ORDER_COUNT],
        normal_free_area: [PhysAddr::new(0); super::super::BUDDY_ORDER_COUNT],
        dma32_empty_area,
        normal_empty_area,
        dma32_free_pages: 0,
        normal_free_pages: 0,
        diagnostics: super::super::BuddyDiagnostics::default(),
        isolation: None,
        next_isolation_id: 1,
        has_normal_memory: false,
        total: PageFrameCount::new(BACKING_PAGES),
        phantom: PhantomData,
    }
}

fn counts(allocator: &BuddyAllocator<MMArch>) -> bool {
    let snapshot = allocator.diagnostic_snapshot();
    snapshot
        .diagnostics
        .zones
        .iter()
        .enumerate()
        .all(|(index, zone)| {
            zone.free_blocks
                .iter()
                .enumerate()
                .map(|(order, blocks)| blocks << order)
                .sum::<usize>()
                == snapshot.free_pages[index]
        })
}

fn split_and_capture(backing: PhysAddr) -> bool {
    let mut allocator = unsafe { empty(backing) };
    let whole = backing + (TARGET_OFFSET << MIN_ORDER);
    unsafe { allocator.free(whole, PageFrameCount::new(TARGET_PAGES)) };
    let target = whole + (128 << MIN_ORDER);
    let mut budget = 100_000;
    let key = allocator.begin_isolation(target, 4, &mut budget).unwrap();
    let initial = allocator.isolated_free_pages(&key) == 16
        && counts(&allocator)
        && allocator
            .diagnostic_snapshot()
            .free_pages
            .iter()
            .sum::<usize>()
            == TARGET_PAGES - 16;
    let outside = unsafe { allocator.allocate(PageFrameCount::ONE) };
    let outside_only = outside.is_some_and(|(base, count)| {
        count == PageFrameCount::ONE && (base < target || base >= target + (16 << MIN_ORDER))
    });
    if let Some((base, count)) = outside {
        unsafe { allocator.free(base, count) };
    }
    let untouched = allocator.isolated_free_pages(&key) == 16;
    let captured = allocator.capture_isolation(&key) == Some(target);
    allocator.release_isolation(&key); // Completed keys cannot release again.
    let absent = !allocator.isolated_free(&key, target);
    let claimed_absent = allocator
        .allocate_in_range(4, target, target + (16 << MIN_ORDER), &mut budget)
        .unwrap()
        .is_none();
    unsafe { allocator.free(target, PageFrameCount::new(16)) };
    initial
        && outside_only
        && untouched
        && captured
        && absent
        && claimed_absent
        && counts(&allocator)
        && allocator
            .allocate_in_range(9, whole, whole + (TARGET_PAGES << MIN_ORDER), &mut budget)
            .unwrap()
            == Some(whole)
}

fn many_metadata_slots(backing: PhysAddr) -> bool {
    let mut allocator = unsafe { empty(backing) };
    let target = backing + (TARGET_OFFSET << MIN_ORDER);
    // Interleave selected and outside entries across two real metadata pages.
    // The newest head ends mostly in nonselected outside entries, so deleting
    // selected entries from the older page exercises head-last replacement.
    for index in 0..TARGET_PAGES / 2 {
        unsafe {
            allocator.free(target + ((2 * index) << MIN_ORDER), PageFrameCount::ONE);
            allocator.free(
                backing + ((128 + 2 * index) << MIN_ORDER),
                PageFrameCount::ONE,
            );
        }
    }
    for index in TARGET_PAGES / 2..300 {
        unsafe {
            allocator.free(
                backing + ((128 + 2 * index) << MIN_ORDER),
                PageFrameCount::ONE,
            )
        };
    }
    let mut budget = 100_000;
    let key = allocator.begin_isolation(target, 9, &mut budget).unwrap();
    let bitmap = (0..TARGET_PAGES).all(|page| {
        allocator.isolated_free(&key, target + (page << MIN_ORDER)) == page.is_multiple_of(2)
    });
    let outside = allocator
        .diagnostic_snapshot()
        .free_pages
        .iter()
        .sum::<usize>()
        == 300;
    for page in (1..TARGET_PAGES).step_by(2) {
        unsafe { allocator.free(target + (page << MIN_ORDER), PageFrameCount::ONE) };
    }
    let captured = allocator.capture_isolation(&key) == Some(target);
    let counted = allocator
        .diagnostic_snapshot()
        .free_pages
        .iter()
        .sum::<usize>()
        == 300;
    let mut seen = vec![false; 300];
    let mut drained = 0;
    while let Some((physical, count)) = unsafe { allocator.allocate(PageFrameCount::ONE) } {
        if count != PageFrameCount::ONE || physical < backing {
            return false;
        }
        let offset = (physical.data() - backing.data()) >> MIN_ORDER;
        if !(128..728).contains(&offset) || !offset.is_multiple_of(2) {
            return false;
        }
        let index = (offset - 128) / 2;
        if seen[index] {
            return false;
        }
        seen[index] = true;
        drained += 1;
    }
    bitmap
        && outside
        && captured
        && counted
        && drained == 300
        && seen.iter().all(|seen| *seen)
        && counts(&allocator)
        && allocator
            .diagnostic_snapshot()
            .free_pages
            .iter()
            .sum::<usize>()
            == 0
}

fn partial_abort_and_stale(backing: PhysAddr) -> bool {
    let mut allocator = unsafe { empty(backing) };
    let target = backing + (TARGET_OFFSET << MIN_ORDER);
    for page in [0, 2, 4, 6] {
        unsafe { allocator.free(target + (page << MIN_ORDER), PageFrameCount::ONE) };
    }
    let mut budget = 100_000;
    let first = allocator.begin_isolation(target, 3, &mut budget).unwrap();
    unsafe { allocator.free(target + (1 << MIN_ORDER), PageFrameCount::ONE) };
    let partial =
        allocator.isolated_free_pages(&first) == 5 && allocator.capture_isolation(&first).is_none();
    allocator.release_isolation(&first);
    let released = allocator
        .diagnostic_snapshot()
        .free_pages
        .iter()
        .sum::<usize>()
        == 5;
    let second = allocator.begin_isolation(target, 3, &mut budget).unwrap();
    allocator.release_isolation(&first);
    let retained =
        allocator.isolated_free_pages(&second) == 5 && allocator.isolated_free_pages(&first) == 0;
    allocator.release_isolation(&second);
    partial && released && retained && counts(&allocator)
}

fn cross_range_free(backing: PhysAddr) -> bool {
    let mut allocator = unsafe { empty(backing) };
    let whole = backing + (TARGET_OFFSET << MIN_ORDER);
    let target = whole + (8 << MIN_ORDER);
    let mut budget = 100_000;
    let key = allocator.begin_isolation(target, 2, &mut budget).unwrap();
    unsafe { allocator.free(whole, PageFrameCount::new(16)) };
    let quarantined = allocator.isolated_free_pages(&key) == 4
        && allocator
            .diagnostic_snapshot()
            .free_pages
            .iter()
            .sum::<usize>()
            == 12;
    allocator.release_isolation(&key);
    quarantined
        && counts(&allocator)
        && allocator
            .allocate_in_range(4, whole, whole + (16 << MIN_ORDER), &mut budget)
            .unwrap()
            == Some(whole)
}

fn budget_no_mutation(backing: PhysAddr) -> bool {
    let target = backing + (TARGET_OFFSET << MIN_ORDER);
    let mut reference = unsafe { empty(backing) };
    unsafe { reference.free(target, PageFrameCount::new(16)) };
    let mut remaining = 100_000;
    let key = reference
        .begin_isolation(target, 3, &mut remaining)
        .unwrap();
    let required = 100_000 - remaining;
    reference.release_isolation(&key);
    // Recreate private metadata to compare all ownership counters immediately
    // before and after a budget exhausted at the last pre-mutation check.
    let mut allocator = unsafe { empty(backing) };
    unsafe { allocator.free(target, PageFrameCount::new(16)) };
    let before = allocator.diagnostic_snapshot();
    let heads = (
        allocator.dma32_free_area,
        allocator.normal_free_area,
        allocator.dma32_empty_area,
        allocator.normal_empty_area,
        allocator.next_isolation_id,
    );
    let result = allocator.begin_isolation(target, 3, &mut (required - 1));
    let after = allocator.diagnostic_snapshot();
    let unchanged = matches!(result, Err(RangeError::Budget))
        && allocator.isolation.is_none()
        && heads
            == (
                allocator.dma32_free_area,
                allocator.normal_free_area,
                allocator.dma32_empty_area,
                allocator.normal_empty_area,
                allocator.next_isolation_id,
            )
        && before.free_pages == after.free_pages
        && counts(&allocator)
        && before
            .diagnostics
            .zones
            .iter()
            .zip(after.diagnostics.zones.iter())
            .all(|(before, after)| before.free_blocks == after.free_blocks);
    let mut exact = required;
    let Ok(key) = allocator.begin_isolation(target, 3, &mut exact) else {
        return false;
    };
    let accepted = exact == 0
        && allocator.isolated_free_pages(&key) == 8
        && allocator
            .diagnostic_snapshot()
            .free_pages
            .iter()
            .sum::<usize>()
            == 8
        && counts(&allocator);
    allocator.release_isolation(&key);
    let mut budget = 100_000;
    unchanged
        && accepted
        && allocator
            .allocate_in_range(4, target, target + (16 << MIN_ORDER), &mut budget)
            .is_ok_and(|block| block == Some(target))
        && counts(&allocator)
}

fn range_subblock_and_validation(backing: PhysAddr) -> bool {
    let mut allocator = unsafe { empty(backing) };
    let whole = backing + (TARGET_OFFSET << MIN_ORDER);
    unsafe { allocator.free(whole, PageFrameCount::new(16)) };
    let target = whole + (4 << MIN_ORDER);
    let mut budget = 100_000;
    let picked = allocator
        .allocate_in_range(2, target, target + (4 << MIN_ORDER), &mut budget)
        .unwrap()
        == Some(target);
    let invalid = allocator.allocate_in_range(0, target, target + (4 << MIN_ORDER), &mut budget)
        == Err(RangeError::Invalid)
        && allocator.allocate_in_range(2, target + 1, target + (4 << MIN_ORDER), &mut budget)
            == Err(RangeError::Invalid)
        && matches!(
            allocator.begin_isolation(target, MAX_COMPACT_ORDER + 1, &mut budget),
            Err(RangeError::Invalid)
        )
        && matches!(
            allocator.begin_isolation(target, 0, &mut budget),
            Err(RangeError::Invalid)
        );
    let overflow = matches!(
        allocator.begin_isolation(
            PhysAddr::new(usize::MAX & !((4 << MIN_ORDER) - 1)),
            2,
            &mut budget
        ),
        Err(RangeError::Invalid)
    );
    picked && invalid && overflow && counts(&allocator)
}

fn active_allocation_is_outside(backing: PhysAddr) -> bool {
    let mut allocator = unsafe { empty(backing) };
    let target = backing + (TARGET_OFFSET << MIN_ORDER);
    let outside = target + (16 << MIN_ORDER);
    unsafe {
        allocator.free(target, PageFrameCount::new(8));
        allocator.free(outside, PageFrameCount::new(8));
    }
    let mut budget = 100_000;
    let key = allocator.begin_isolation(target, 3, &mut budget).unwrap();
    let busy = matches!(
        allocator.begin_isolation(outside, 3, &mut budget),
        Err(RangeError::Busy)
    );
    let allocated = unsafe { allocator.allocate(PageFrameCount::new(8)) };
    let remained = allocator.isolated_free_pages(&key) == 8;
    allocator.release_isolation(&key);
    busy && remained
        && allocated.is_some_and(|(base, count)| base == outside && count.data() == 8)
        && counts(&allocator)
}

fn zone_models_and_reuse(backing: PhysAddr) -> bool {
    let mut allocator = unsafe { empty(backing) };
    // Only free-list addresses are modelled here, as in the existing buddy
    // DMA32 selftest. Metadata is real backed RAM; neither data PFN is read.
    let low = PhysAddr::new(0x0200_0000);
    let high = PhysAddr::new((1usize << 32) + 0x0200_0000);
    unsafe {
        allocator.free(low, PageFrameCount::new(8));
        allocator.free(high, PageFrameCount::new(8));
    }
    let mut budget = 100_000;
    let key = allocator.begin_isolation(low, 3, &mut budget).unwrap();
    let normal = unsafe { allocator.allocate(PageFrameCount::new(8)) };
    let isolated = allocator.capture_isolation(&key) == Some(low);
    let next = allocator.begin_isolation(high, 3, &mut budget).unwrap();
    unsafe { allocator.free(high, PageFrameCount::new(8)) };
    let normal_capture = allocator.capture_isolation(&next) == Some(high);
    unsafe { allocator.free(low, PageFrameCount::new(8)) };
    let low_capture = allocator
        .allocate_in_range(3, low, low + (8 << MIN_ORDER), &mut budget)
        .unwrap()
        == Some(low);
    normal.is_some_and(|(base, count)| base == high && count.data() == 8)
        && isolated
        && normal_capture
        && low_capture
        && counts(&allocator)
}

// Model distant free PFNs without touching their contents, as in zone_models.
// Only metadata and the selected target use the exclusively owned backing.
fn foreign_address(base: PhysAddr, ordinal: usize, target: PhysAddr) -> PhysAddr {
    let address = base + (ordinal << (MIN_ORDER + 1));
    if address >= target && address < target + (TARGET_PAGES << MIN_ORDER) {
        address + (1 << 28)
    } else {
        address
    }
}

fn large_single_scan(backing: PhysAddr) -> bool {
    const NODES: usize = 18;
    let mut allocator = unsafe { empty(backing) };
    let target = backing + (TARGET_OFFSET << MIN_ORDER);
    let zone = BuddyAllocator::<MMArch>::zone_for(target, MIN_ORDER + 3);
    let foreign_base = PhysAddr::new(if zone == BuddyZone::Normal {
        (1usize << 32) + (1 << 28)
    } else {
        1 << 28
    });
    let capacity = BuddyAllocator::<MMArch>::BUDDY_ENTRIES;
    // A partial head and E-1 entries in every non-head exercise the tight
    // metadata bound, including head recycling during replacement donations.
    let entries = 4 + (NODES - 1) * (capacity - 1);
    let selected = [1, 3, 4 + 7 * (capacity - 1), entries - (capacity - 1)];
    let mut ordinal = 0;
    for node in 0..NODES {
        let metadata = backing + ((64 + node) << MIN_ORDER);
        let next = if node + 1 == NODES {
            PhysAddr::new(0)
        } else {
            metadata + (1 << MIN_ORDER)
        };
        let length = if node == 0 { 4 } else { capacity - 1 };
        BuddyAllocator::<MMArch>::write_page(metadata, PageList::new(length, next));
        for index in 0..length {
            let address = if let Some(page) = selected.iter().position(|&slot| slot == ordinal) {
                target + ((2 * page) << MIN_ORDER)
            } else {
                foreign_address(foreign_base, ordinal, target)
            };
            unsafe {
                MMArch::write(
                    BuddyAllocator::<MMArch>::entry_virt_addr(metadata, index),
                    address,
                )
            };
            ordinal += 1;
        }
    }
    allocator.set_free_area(zone, MIN_ORDER as u8, backing + (64 << MIN_ORDER));
    allocator.diagnostics.zones[zone.index()].free_blocks[0] = entries;
    allocator.adjust_free_pages(zone, entries, true);
    let required = allocator.isolation_work_bound(zone, MIN_ORDER + 3).unwrap();
    // The former two complete scans cannot fit, but one bounded scan can.
    let old_minimum = 2 * ((MAX_ORDER - MIN_ORDER) + entries + NODES) + selected.len();
    if entries <= 8192 || old_minimum <= 16384 || required > 16384 {
        return false;
    }
    unsafe {
        core::ptr::write_bytes(
            MMArch::phys_2_virt(target).unwrap().as_ptr::<u8>(),
            0x5a,
            8 << MIN_ORDER,
        )
    };
    let before = allocator.diagnostic_snapshot();
    let heads = (allocator.dma32_free_area, allocator.normal_free_area);
    let id = allocator.next_isolation_id;
    let mut short = required - 1;
    let rejected = matches!(
        allocator.begin_isolation(target, 3, &mut short),
        Err(RangeError::Budget)
    );
    let after = allocator.diagnostic_snapshot();
    let untouched = rejected
        && short == 0
        && allocator.isolation.is_none()
        && allocator.next_isolation_id == id
        && heads == (allocator.dma32_free_area, allocator.normal_free_area)
        && before.free_pages == after.free_pages
        && before
            .diagnostics
            .zones
            .iter()
            .zip(after.diagnostics.zones.iter())
            .all(|(left, right)| left.free_blocks == right.free_blocks)
        && unsafe {
            core::slice::from_raw_parts(
                MMArch::phys_2_virt(target).unwrap().as_ptr::<u8>(),
                8 << MIN_ORDER,
            )
        }
        .iter()
        .all(|&byte| byte == 0x5a);
    let mut exact = required;
    let Ok(key) = allocator.begin_isolation(target, 3, &mut exact) else {
        return false;
    };
    let bitmap = exact == 0
        && (0..8).all(|page| {
            allocator.isolated_free(&key, target + (page << MIN_ORDER)) == page.is_multiple_of(2)
        });
    for page in (1..8).step_by(2) {
        unsafe { allocator.free(target + (page << MIN_ORDER), PageFrameCount::ONE) };
    }
    let captured = allocator.capture_isolation(&key) == Some(target);
    let mut seen = vec![false; entries];
    for &slot in &selected {
        seen[slot] = true;
    }
    let mut drained = 0;
    while let Some(address) = allocator.pop_front(zone, MIN_ORDER as u8) {
        if address < foreign_base {
            return false;
        }
        let mut offset = address.data() - foreign_base.data();
        if offset >= 1 << 28 {
            offset -= 1 << 28;
        }
        let index = offset >> (MIN_ORDER + 1);
        if index >= entries
            || seen[index]
            || foreign_address(foreign_base, index, target) != address
        {
            return false;
        }
        seen[index] = true;
        drained += 1;
    }
    untouched
        && bitmap
        && captured
        && drained == entries - selected.len()
        && seen.iter().all(|&value| value)
        && counts(&allocator)
        && allocator
            .diagnostic_snapshot()
            .free_pages
            .iter()
            .sum::<usize>()
            == 0
}

fn split_without_spare_metadata(backing: PhysAddr) -> bool {
    let mut allocator = unsafe { empty(backing) };
    let target = backing + (TARGET_OFFSET << MIN_ORDER);
    let zone = BuddyAllocator::<MMArch>::zone_for(target, MIN_ORDER + 2);
    let capacity = BuddyAllocator::<MMArch>::BUDDY_ENTRIES;
    let foreign_base = PhysAddr::new(if zone == BuddyZone::Normal {
        (1usize << 32) + (1 << 28)
    } else {
        1 << 28
    });
    let metadata = backing + (64 << MIN_ORDER);
    let metadata_source = backing + (128 << MIN_ORDER);
    BuddyAllocator::<MMArch>::write_page(metadata, PageList::new(capacity, PhysAddr::new(0)));
    for index in 0..capacity {
        let address = if index + 1 == capacity {
            metadata_source
        } else {
            foreign_address(foreign_base, 2 * index, target)
        };
        unsafe {
            MMArch::write(
                BuddyAllocator::<MMArch>::entry_virt_addr(metadata, index),
                address,
            )
        };
    }
    allocator.set_free_area(zone, (MIN_ORDER + 1) as u8, metadata);
    allocator.set_empty_area(zone, (MIN_ORDER + 1) as u8, PhysAddr::new(0));
    allocator.diagnostics.zones[zone.index()].free_blocks[1] = capacity;
    allocator.adjust_free_pages(zone, 2 * capacity, true);
    unsafe { allocator.free(target, PageFrameCount::new(4)) };
    let mut budget = 16384;
    let Ok(key) = allocator.begin_isolation(target, 1, &mut budget) else {
        return false;
    };
    // Returning the outside half fills a full order with no spare metadata:
    // one real two-page block becomes metadata plus an order-0 remainder.
    let accounted = allocator.isolated_free_pages(&key) == 2
        && allocator.free_area(zone, (MIN_ORDER + 1) as u8) == metadata_source
        && allocator
            .diagnostic_snapshot()
            .free_pages
            .iter()
            .sum::<usize>()
            == 2 * capacity + 1
        && counts(&allocator);
    let captured = allocator.capture_isolation(&key) == Some(target);
    let outside = target + (2 << MIN_ORDER);
    accounted
        && captured
        && allocator
            .allocate_in_range(1, outside, outside + (2 << MIN_ORDER), &mut budget)
            .is_ok_and(|block| block == Some(outside))
        && counts(&allocator)
}

pub(crate) fn checks() -> Vec<(&'static str, bool)> {
    let Some((base, count)) = (unsafe {
        LockedFrameAllocator.allocate_below(
            PageFrameCount::new(BACKING_PAGES),
            PhysAddr::new(usize::MAX),
        )
    }) else {
        return vec![("buddy_compaction_backing", false)];
    };
    assert_eq!(count.data(), BACKING_PAGES);
    let _backing = Backing(base);
    vec![
        ("buddy_compaction_split_capture", split_and_capture(base)),
        ("buddy_compaction_metadata_slots", many_metadata_slots(base)),
        ("buddy_compaction_single_scan", large_single_scan(base)),
        (
            "buddy_compaction_split_metadata",
            split_without_spare_metadata(base),
        ),
        (
            "buddy_compaction_abort_stale",
            partial_abort_and_stale(base),
        ),
        ("buddy_compaction_cross_free", cross_range_free(base)),
        ("buddy_compaction_budget_rollback", budget_no_mutation(base)),
        (
            "buddy_compaction_range_validation",
            range_subblock_and_validation(base),
        ),
        (
            "buddy_compaction_alloc_exclusion",
            active_allocation_is_outside(base),
        ),
        ("buddy_compaction_zone_reuse", zone_models_and_reuse(base)),
    ]
}
