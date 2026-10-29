//! Real private buddy churn. The all-unmovable control uses the same allocator,
//! metadata and operation sequence; only the short-lived placement hint differs.
use super::*;
use alloc::{string::String, vec::Vec};
use core::fmt::Write;

const BACKING_PAGES: usize = 8192;
const ARENA_PAGES: usize = 4096;
const OPERATIONS: usize = 3072;
const BLOCK_PAGES: usize = 1 << (BLOCK_ORDER - MIN_ORDER);

struct Backing(PhysAddr, PageFrameCount);
impl Drop for Backing {
    fn drop(&mut self) {
        unsafe { deallocate_page_frames(PhysPageFrame::new(self.0), self.1) };
    }
}

#[derive(Default)]
struct Churn {
    valid: bool,
    complete_blocks: usize,
    pollution: u64,
    metadata_pages: usize,
}

unsafe fn boundaries(backing: PhysAddr) -> Option<bool> {
    let arena = backing + 16 * 1024 * 1024;
    let mut buddy = BuddyAllocator::<MMArch>::placement_fixture(backing, arena, 2 * BLOCK_PAGES)?;
    buddy.free(arena, PageFrameCount::new(BLOCK_PAGES));
    let request = super::super::super::page_frame::FrameAllocationRequest::kernel(
        PageFrameCount::ONE,
        PhysAddr::new(usize::MAX),
    );
    let movable = request.with_placement(FramePlacement::Movable);
    let (first, _) = buddy.allocate_admitted(movable, ReserveLevel::Reclaim)?;
    let mut valid = buddy.placement.get::<MMArch>(first) == FramePlacement::Movable;
    // The only remaining extent is partially occupied and Movable. Unmovable
    // fallback must use it without retagging its existing live owner.
    let (fallback, _) = buddy.allocate_admitted(request, ReserveLevel::Reclaim)?;
    valid &= buddy.placement.get::<MMArch>(fallback) == FramePlacement::Movable;
    let before = buddy.diagnostic_snapshot().free_pages;
    let mut no_budget = 0;
    valid &= matches!(
        buddy.begin_isolation(arena, 9, &mut no_budget),
        Err(compaction::RangeError::Budget)
    );
    valid &= buddy.diagnostic_snapshot().free_pages == before;
    // An unrelated U free page must survive the same zone's M-chain scan.
    let outside_page = arena + (1 << BLOCK_ORDER);
    buddy.free(outside_page, PageFrameCount::ONE);
    let mut budget = 65536;
    let key = buddy.begin_isolation(arena, 9, &mut budget).ok()?;
    buddy.free(first, PageFrameCount::ONE);
    buddy.free(fallback, PageFrameCount::ONE);
    let (unrelated, _) = buddy.allocate_admitted(request, ReserveLevel::Reclaim)?;
    valid &= unrelated == outside_page;
    buddy.free(unrelated, PageFrameCount::ONE);
    valid &= buddy.capture_isolation(&key) == Some(arena);
    buddy.free(arena, PageFrameCount::new(BLOCK_PAGES));
    valid &= counts_match(&buddy);
    // A limited request cannot escape the address domain to preserve a class.
    let outside = super::super::super::page_frame::FrameAllocationRequest::kernel(
        PageFrameCount::ONE,
        arena - 1,
    )
    .with_placement(FramePlacement::Movable);
    valid &= buddy
        .allocate_admitted(outside, ReserveLevel::Reclaim)
        .is_none();
    let zone = BuddyAllocator::<MMArch>::zone_for(arena, MIN_ORDER);
    buddy.watermarks[zone.index()].min = BLOCK_PAGES + 1;
    valid &= buddy
        .allocate_admitted(movable, ReserveLevel::Kernel)
        .is_none();
    buddy.watermarks[zone.index()].min = 0;
    let whole = super::super::super::page_frame::FrameAllocationRequest::kernel(
        PageFrameCount::new(BLOCK_PAGES),
        PhysAddr::new(usize::MAX),
    );
    valid &= buddy
        .allocate_admitted(whole, ReserveLevel::Reclaim)
        .map(|value| value.0)
        == Some(arena);
    Some(valid)
}

unsafe fn metadata_growth(backing: PhysAddr) -> Option<bool> {
    let arena = backing + 16 * 1024 * 1024;
    let mut buddy = BuddyAllocator::<MMArch>::placement_fixture(backing, arena, 2 * BLOCK_PAGES)?;
    // This entire private block is exclusively held, so it may be labeled M.
    // Keep its first page live while publishing a disjoint two-page free run.
    buddy
        .placement
        .set::<MMArch>(arena, BLOCK_ORDER, FramePlacement::Movable);
    let zone = BuddyAllocator::<MMArch>::zone_for(arena, MIN_ORDER);
    let order = (MIN_ORDER + 1) as u8;
    buddy.set_empty_area(zone, order, PhysAddr::new(0));
    buddy.insert_without_merge(arena + 2 * MMArch::PAGE_SIZE, order);
    let mut valid = buddy.metadata_from_movable == 1
        && buddy.zone_free(zone) == 1
        && counts_match(&buddy)
        && buddy.placement.get::<MMArch>(arena) == FramePlacement::Movable;
    // The other class reuses the same shared metadata page without consuming
    // another frame or changing either physical block's label.
    let unmovable = arena + (1 << BLOCK_ORDER) + 4 * MMArch::PAGE_SIZE;
    buddy.insert_without_merge(unmovable, order);
    valid &= buddy.metadata_from_movable == 1
        && buddy.zone_free(zone) == 3
        && counts_match(&buddy)
        && buddy.placement.get::<MMArch>(arena) == FramePlacement::Movable
        && buddy.placement.get::<MMArch>(unmovable) == FramePlacement::Unmovable;
    Some(valid)
}

fn counts_match(buddy: &BuddyAllocator<MMArch>) -> bool {
    let snapshot = buddy.diagnostic_snapshot();
    snapshot
        .diagnostics
        .zones
        .iter()
        .enumerate()
        .all(|(zone, stats)| {
            stats
                .free_blocks
                .iter()
                .enumerate()
                .map(|(order, count)| count << order)
                .sum::<usize>()
                == snapshot.free_pages[zone]
        })
}

unsafe fn churn(backing: PhysAddr, marked: bool) -> Option<Churn> {
    // Both extents are aligned because the backing is a real 32 MiB buddy run.
    let arena = backing + 16 * 1024 * 1024;
    let mut buddy = BuddyAllocator::<MMArch>::placement_fixture(backing, arena, ARENA_PAGES)?;
    buddy.free(arena, PageFrameCount::new(ARENA_PAGES));
    let mut live = Vec::new();
    live.try_reserve_exact(OPERATIONS).ok()?;
    let request = super::super::super::page_frame::FrameAllocationRequest::kernel(
        PageFrameCount::ONE,
        PhysAddr::new(usize::MAX),
    );
    for index in 0..OPERATIONS {
        let class = if marked && index % 2 != 0 {
            FramePlacement::Movable
        } else {
            FramePlacement::Unmovable
        };
        let (physical, count) =
            buddy.allocate_admitted(request.with_placement(class), ReserveLevel::Reclaim)?;
        if count != PageFrameCount::ONE {
            return None;
        }
        MMArch::write(MMArch::phys_2_virt(physical)?, index as u64 ^ 0x265);
        live.push(physical);
    }
    // Release the same short-lived owners in a reproducible scrambled order.
    // The odd multiplier permutes the power-of-two-sized first 1024 pairs;
    // the final 512 pairs are then released in reverse order.
    for step in 0..1024 {
        let pair = (step * 109) & 1023;
        buddy.free(live[pair * 2 + 1], PageFrameCount::ONE);
    }
    for pair in (1024..OPERATIONS / 2).rev() {
        buddy.free(live[pair * 2 + 1], PageFrameCount::ONE);
    }
    let mut valid = counts_match(&buddy);
    for (index, physical) in live.iter().enumerate().step_by(2) {
        valid &= MMArch::read::<u64>(MMArch::phys_2_virt(*physical)?) == index as u64 ^ 0x265;
    }
    let pollution = buddy.metadata_from_movable;
    let mut captured = Vec::new();
    captured.try_reserve_exact(ARENA_PAGES / BLOCK_PAGES).ok()?;
    let large = super::super::super::page_frame::FrameAllocationRequest::kernel(
        PageFrameCount::new(BLOCK_PAGES),
        PhysAddr::new(usize::MAX),
    );
    while let Some((physical, count)) = buddy.allocate_admitted(large, ReserveLevel::Reclaim) {
        valid &= count.data() == BLOCK_PAGES && physical.check_aligned(1 << BLOCK_ORDER);
        captured.push(physical);
        if captured.len() > ARENA_PAGES / BLOCK_PAGES {
            return None;
        }
    }
    let complete_blocks = captured.len();
    for physical in captured {
        buddy.free(physical, PageFrameCount::new(BLOCK_PAGES));
    }
    for physical in live.into_iter().step_by(2) {
        buddy.free(physical, PageFrameCount::ONE);
    }
    valid &= counts_match(&buddy);
    let free = buddy.dma32_free_pages + buddy.normal_free_pages;
    let metadata_pages = ARENA_PAGES.checked_sub(free)?;
    // Metadata consumed from the arena remains owned by the private allocator,
    // and is released together with the backing. It is never silently counted
    // as a recoverable data frame.
    // This is this fixed workload's storage budget, not a bound on all
    // metadata accumulated by an arbitrarily long allocator lifetime.
    valid &= metadata_pages <= 32;
    Some(Churn {
        valid,
        complete_blocks,
        pollution,
        metadata_pages,
    })
}

pub(super) fn report() -> String {
    let mut report = String::new();
    let result = unsafe { allocate_page_frames(PageFrameCount::new(BACKING_PAGES)) };
    let trials = result.and_then(|(base, count)| {
        let backing = Backing(base, count);
        if count.data() != BACKING_PAGES {
            return None;
        }
        let control = unsafe { churn(backing.0, false) }?;
        let grouped = unsafe { churn(backing.0, true) }?;
        let boundaries = unsafe { boundaries(backing.0) }?;
        let metadata = unsafe { metadata_growth(backing.0) }?;
        let map = super::map_selftest::run(backing.0, backing.0 + 16 * 1024 * 1024);
        Some((control, grouped, boundaries, metadata, map))
    });
    let churn_ok = trials.as_ref().is_some_and(|(control, grouped, _, _, _)| {
        control.valid && grouped.valid && grouped.complete_blocks > control.complete_blocks
    });
    let boundaries_ok = trials.as_ref().is_some_and(|(_, _, passed, _, _)| *passed);
    let metadata_ok = trials.as_ref().is_some_and(|(_, _, _, passed, _)| *passed);
    if let Some((control, grouped, _, _, _)) = &trials {
        writeln!(
            report,
            "control_complete_blocks={}",
            control.complete_blocks
        )
        .unwrap();
        writeln!(
            report,
            "grouped_complete_blocks={}",
            grouped.complete_blocks
        )
        .unwrap();
        writeln!(report, "control_metadata_pages={}", control.metadata_pages).unwrap();
        writeln!(report, "grouped_metadata_pages={}", grouped.metadata_pages).unwrap();
        writeln!(report, "metadata_from_movable={}", grouped.pollution).unwrap();
    }
    let map_ok = trials.as_ref().is_some_and(|(_, _, _, _, passed)| *passed);
    writeln!(
        report,
        "placement_map={}",
        if map_ok { "ok" } else { "fail" }
    )
    .unwrap();
    let boot_ok = crate::mm::memblock::reservation_selftest();
    let consumer_checks = crate::mm::page::placement_selftest::checks();
    let consumers_ok = consumer_checks.iter().all(|(_, passed)| *passed);
    for (name, passed) in consumer_checks {
        writeln!(
            report,
            "consumer_{name}={}",
            if passed { "ok" } else { "fail" }
        )
        .unwrap();
    }
    writeln!(
        report,
        "consumer_births={}",
        if consumers_ok { "ok" } else { "fail" }
    )
    .unwrap();
    writeln!(
        report,
        "placement_churn={}",
        if churn_ok { "ok" } else { "fail" }
    )
    .unwrap();
    writeln!(
        report,
        "boot_reservation={}",
        if boot_ok { "ok" } else { "fail" }
    )
    .unwrap();
    writeln!(
        report,
        "placement_boundaries={}",
        if boundaries_ok { "ok" } else { "fail" }
    )
    .unwrap();
    writeln!(
        report,
        "placement_metadata={}",
        if metadata_ok { "ok" } else { "fail" }
    )
    .unwrap();
    let failures = usize::from(!churn_ok)
        + usize::from(!boot_ok)
        + usize::from(!boundaries_ok)
        + usize::from(!metadata_ok)
        + usize::from(!consumers_ok)
        + usize::from(!map_ok);
    writeln!(
        report,
        "status={}",
        if failures == 0 { "ok" } else { "fail" }
    )
    .unwrap();
    writeln!(report, "summary_fail={failures}").unwrap();
    report
}
