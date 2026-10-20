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
    let mut scratch = IsolationScratch::try_new(4).unwrap();
    let mut budget = 100_000;
    let key = allocator
        .begin_isolation(target, 4, &mut scratch, &mut budget)
        .unwrap();
    let initial = allocator.isolated_free_pages(&key) == 16 && counts(&allocator);
    let captured = allocator.capture_isolation(&key) == Some(target);
    allocator.release_isolation(&key); // Completed keys cannot release again.
    let absent = !allocator.isolated_free(&key, target);
    let claimed_absent = allocator
        .allocate_in_range(4, target, target + (16 << MIN_ORDER), &mut budget)
        .unwrap()
        .is_none();
    unsafe { allocator.free(target, PageFrameCount::new(16)) };
    initial
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
    // The selected target entries straddle the two real metadata pages; all
    // outside entries must survive the head-last swap removal unchanged.
    for page in (128..728).step_by(2) {
        unsafe { allocator.free(backing + (page << MIN_ORDER), PageFrameCount::ONE) };
    }
    for page in (0..TARGET_PAGES).step_by(2) {
        unsafe { allocator.free(target + (page << MIN_ORDER), PageFrameCount::ONE) };
    }
    let mut scratch = IsolationScratch::try_new(9).unwrap();
    let mut budget = 100_000;
    let key = allocator
        .begin_isolation(target, 9, &mut scratch, &mut budget)
        .unwrap();
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
    bitmap
        && outside
        && counts(&allocator)
        && allocator.capture_isolation(&key) == Some(target)
        && allocator
            .diagnostic_snapshot()
            .free_pages
            .iter()
            .sum::<usize>()
            == 300
}

fn partial_abort_and_stale(backing: PhysAddr) -> bool {
    let mut allocator = unsafe { empty(backing) };
    let target = backing + (TARGET_OFFSET << MIN_ORDER);
    for page in [0, 2, 4, 6] {
        unsafe { allocator.free(target + (page << MIN_ORDER), PageFrameCount::ONE) };
    }
    let mut scratch = IsolationScratch::try_new(3).unwrap();
    let mut budget = 100_000;
    let first = allocator
        .begin_isolation(target, 3, &mut scratch, &mut budget)
        .unwrap();
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
    let second = allocator
        .begin_isolation(target, 3, &mut scratch, &mut budget)
        .unwrap();
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
    let mut scratch = IsolationScratch::try_new(2).unwrap();
    let mut budget = 100_000;
    let key = allocator
        .begin_isolation(target, 2, &mut scratch, &mut budget)
        .unwrap();
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
    let mut scratch = IsolationScratch::try_new(3).unwrap();
    let mut remaining = 100_000;
    let key = reference
        .begin_isolation(target, 3, &mut scratch, &mut remaining)
        .unwrap();
    let required = 100_000 - remaining;
    reference.release_isolation(&key);
    // Recreate private metadata to compare all ownership counters immediately
    // before and after a budget exhausted at the last pre-mutation check.
    let mut allocator = unsafe { empty(backing) };
    unsafe { allocator.free(target, PageFrameCount::new(16)) };
    let before = allocator.diagnostic_snapshot();
    let result = allocator.begin_isolation(target, 3, &mut scratch, &mut (required - 1));
    let after = allocator.diagnostic_snapshot();
    matches!(result, Err(RangeError::Budget))
        && allocator.isolation.is_none()
        && before.free_pages == after.free_pages
        && counts(&allocator)
        && before
            .diagnostics
            .zones
            .iter()
            .zip(after.diagnostics.zones.iter())
            .all(|(before, after)| before.free_blocks == after.free_blocks)
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
        && IsolationScratch::try_new(MAX_COMPACT_ORDER + 1).is_err();
    let mut scratch = IsolationScratch::try_new(2).unwrap();
    let overflow = matches!(
        allocator.begin_isolation(
            PhysAddr::new(usize::MAX & !((4 << MIN_ORDER) - 1)),
            2,
            &mut scratch,
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
    let mut scratch = IsolationScratch::try_new(3).unwrap();
    let mut budget = 100_000;
    let key = allocator
        .begin_isolation(target, 3, &mut scratch, &mut budget)
        .unwrap();
    let busy = matches!(
        allocator.begin_isolation(outside, 3, &mut scratch, &mut budget),
        Err(RangeError::Busy)
    );
    let allocated = unsafe { allocator.allocate(PageFrameCount::new(8)) };
    let remained = allocator.isolated_free_pages(&key) == 8;
    allocator.release_isolation(&key);
    busy && remained
        && allocated.is_some_and(|(base, count)| base == outside && count.data() == 8)
        && counts(&allocator)
}

fn zone_models_and_scratch_reuse(backing: PhysAddr) -> bool {
    let mut allocator = unsafe { empty(backing) };
    // Only free-list addresses are modelled here, as in the existing buddy
    // DMA32 selftest. Metadata is real backed RAM; neither data PFN is read.
    let low = PhysAddr::new(0x0200_0000);
    let high = PhysAddr::new((1usize << 32) + 0x0200_0000);
    unsafe {
        allocator.free(low, PageFrameCount::new(8));
        allocator.free(high, PageFrameCount::new(8));
    }
    let mut scratch = IsolationScratch::try_new(9).unwrap();
    let mut budget = 100_000;
    let key = allocator
        .begin_isolation(low, 3, &mut scratch, &mut budget)
        .unwrap();
    let normal = unsafe { allocator.allocate(PageFrameCount::new(8)) };
    let isolated = allocator.capture_isolation(&key) == Some(low);
    let next = allocator
        .begin_isolation(high, 3, &mut scratch, &mut budget)
        .unwrap();
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
        (
            "buddy_compaction_zone_scratch_reuse",
            zone_models_and_scratch_reuse(base),
        ),
    ]
}
