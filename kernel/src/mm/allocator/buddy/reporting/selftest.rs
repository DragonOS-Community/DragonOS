//! Exercise the real buddy implementation over exclusively owned RAM.
use super::*;
use crate::{
    arch::{mm::LockedFrameAllocator, MMArch},
    mm::allocator::page_frame::FrameAllocator,
};
use alloc::string::String;
use core::{fmt::Write, marker::PhantomData};

const BACKING: usize = 4096;
const BLOCK_ORDER: u8 = 22;
const BLOCK_PAGES: usize = 1 << (BLOCK_ORDER as usize - MIN_ORDER);

struct Backing(PhysAddr);
impl Drop for Backing {
    fn drop(&mut self) {
        unsafe { LockedFrameAllocator.free(self.0, PageFrameCount::new(BACKING)) };
    }
}

// Keep metadata-only synthetic free heads disjoint from the real arena in
// either address zone. A 16MiB-aligned arena cannot cross this 2GiB boundary.
fn foreign_base(base: PhysAddr) -> usize {
    let zone = if base.data() >= 1usize << 32 {
        1usize << 32
    } else {
        0
    };
    let middle = zone + (1usize << 31);
    if base.data() < middle {
        middle
    } else {
        zone + (1 << BLOCK_ORDER)
    }
}

fn reporting_index(base: PhysAddr) -> super::super::free_index::FreeIndex<MMArch> {
    use crate::mm::{memblock::MemoryAreaAttr, PhysMemoryArea};
    let mut areas = alloc::vec![
        PhysMemoryArea::new(base, BACKING << MIN_ORDER, MemoryAreaAttr::empty()),
        PhysMemoryArea::new(
            PhysAddr::new(foreign_base(base)),
            (BuddyAllocator::<MMArch>::BUDDY_ENTRIES + 2) << BLOCK_ORDER,
            MemoryAreaAttr::empty(),
        ),
    ];
    if base.data() >= 1usize << 32 {
        // The address-zone reserve model adds this separate low-memory key.
        areas.push(PhysMemoryArea::new(
            PhysAddr::new(0x2000_0000),
            1 << BLOCK_ORDER,
            MemoryAreaAttr::empty(),
        ));
    }
    areas.sort_unstable_by_key(|area| area.base);
    assert!(areas
        .windows(2)
        .all(|pair| pair[0].base + pair[0].size <= pair[1].base));
    super::super::free_index::FreeIndex::fixture(&areas).expect("reporting fixture index")
}

unsafe fn fresh(base: PhysAddr) -> BuddyAllocator<MMArch> {
    let mut empty = [[PhysAddr::new(0); BUDDY_ORDER_COUNT]; 2];
    for (index, slot) in empty.iter_mut().flatten().enumerate() {
        *slot = base + (index << MIN_ORDER);
        BuddyAllocator::<MMArch>::write_page(*slot, PageList::new(0, PhysAddr::new(0)));
    }
    let mut buddy = BuddyAllocator {
        free_index: reporting_index(base),
        dma32_free_area: [PhysAddr::new(0); BUDDY_ORDER_COUNT],
        normal_free_area: [PhysAddr::new(0); BUDDY_ORDER_COUNT],
        movable_free_area: [[PhysAddr::new(0); super::super::LOW_ORDERS]; 2],
        movable_blocks: [[0; super::super::LOW_ORDERS]; 2],
        placement: super::super::PlacementMap::empty(),
        metadata_from_movable: 0,
        dma32_empty_area: empty[0],
        normal_empty_area: empty[1],
        dma32_free_pages: 0,
        normal_free_pages: 0,
        watermarks: [super::super::super::watermark::ZoneWatermarks::default(); 2],
        diagnostics: super::super::BuddyDiagnostics::default(),
        isolation: None,
        next_isolation_id: 1,
        reporting: ReportingState::new(),
        has_normal_memory: false,
        total: PageFrameCount::new(BACKING),
        phantom: PhantomData,
    };
    // Small inventory serves the actual one-page spare allocation without
    // changing the large block whose report/reuse semantics are under test.
    buddy.buddy_free(base + (64 << MIN_ORDER), 18);
    buddy.buddy_free(base + (1024 << MIN_ORDER), BLOCK_ORDER);
    buddy
}

fn run(base: PhysAddr) -> [bool; 6] {
    let mut buddy = unsafe { fresh(base) };
    let before = buddy.dma32_free_pages + buddy.normal_free_pages;
    let Some(block) = buddy.take_reporting_block() else {
        return [false; 6];
    };
    let address = block.address();
    let zone = BuddyAllocator::<MMArch>::zone_for(address, BLOCK_ORDER as usize);
    let ownership = block.bytes() == BLOCK_PAGES << MIN_ORDER
        && buddy.free_entry(address).is_none()
        && buddy.take_reporting_block().is_none()
        && buddy
            .buddy_alloc_with_level(PageFrameCount::new(512), ReserveLevel::User)
            .is_err()
        && buddy.dma32_free_pages + buddy.normal_free_pages == before - BLOCK_PAGES - 1;
    // Simulate MADV_DONTNEED changing all bytes, including the first page.
    unsafe {
        core::ptr::write_bytes(
            MMArch::phys_2_virt(address).unwrap().as_ptr::<u8>(),
            0,
            block.bytes(),
        )
    };
    buddy.finish_reporting_block(block, true);
    let discard_putback = buddy
        .free_entry(address)
        .is_some_and(|(order, _, _)| order == BLOCK_ORDER)
        && buddy.dma32_free_pages + buddy.normal_free_pages == before - 1
        && buddy.take_reporting_block().is_none();

    // Allocation decodes flagged addresses and splits clear the previous flag.
    let mut reuse = false;
    if let Ok((first, count)) =
        buddy.buddy_alloc_with_level(PageFrameCount::new(512), ReserveLevel::User)
    {
        if let Some(other) = buddy.take_reporting_block() {
            reuse =
                first == address && other.address() != first && other.bytes() == 2 * 1024 * 1024;
            buddy.finish_reporting_block(other, true);
        }
        unsafe { buddy.free(first, count) };
        if let Some(merged) = buddy.take_reporting_block() {
            reuse &= merged.address() == address && merged.bytes() == BLOCK_PAGES << MIN_ORDER;
            buddy.finish_reporting_block(merged, false);
        } else {
            reuse = false;
        }
    }
    let free = buddy.zone_free(zone);
    buddy.watermarks[zone.index()].low = free - BLOCK_PAGES + 1;
    let low_reserve = buddy.take_reporting_block().is_none() && buddy.zone_free(zone) == free;
    buddy.watermarks[zone.index()].low = 0;

    // A machine which has managed Normal memory must retain its DMA32 fallback
    // inventory even when the per-zone low watermark would admit reporting.
    let mut dma_reserve = unsafe { fresh(base) };
    let mut dma32_reserve = false;
    if let Ok((spare, _)) =
        dma_reserve.buddy_alloc_with_level(PageFrameCount::ONE, ReserveLevel::User)
    {
        dma_reserve.reporting.spare = spare;
        if zone == BuddyZone::Normal {
            // Real backing can be above 4GiB; the synthetic low-memory block
            // is metadata-only and, with the correct guard, never leaves MM.
            unsafe { dma_reserve.insert_without_merge(PhysAddr::new(0x2000_0000), BLOCK_ORDER) };
            dma_reserve.watermarks[BuddyZone::Normal.index()].low = dma_reserve.normal_free_pages;
        }
        dma_reserve.has_normal_memory = true;
        let free = dma_reserve.dma32_free_pages;
        dma32_reserve = !dma_reserve.dma32_fallback_allowed(BLOCK_PAGES)
            && dma_reserve.take_reporting_block().is_none()
            && dma_reserve.dma32_free_pages == free;
        // The balloon's wide PFN limit must preserve the same fallback floor
        // as an unrestricted User request, rather than become a DMA privilege.
        let pfn_limit = PhysAddr::new(((u32::MAX as u64 + 1) * 4096 - 1) as usize);
        let user = super::super::super::page_frame::FrameAllocationRequest::user(
            PageFrameCount::ONE,
            pfn_limit,
        );
        dma32_reserve &= dma_reserve
            .allocate_admitted(user, ReserveLevel::User)
            .is_none()
            && dma_reserve.dma32_free_pages == free;
        // Make Normal unavailable to both authorities so the following
        // success proves that explicit Kernel/DMA access remains supported.
        dma_reserve.watermarks[BuddyZone::Normal.index()].min = dma_reserve.normal_free_pages;
        let kernel = dma_reserve.buddy_alloc_below_with_level(
            PageFrameCount::ONE,
            pfn_limit,
            ReserveLevel::Kernel,
        );
        dma32_reserve &= kernel
            .as_ref()
            .is_ok_and(|(address, _)| address.data() <= super::super::DMA32_LIMIT);
        if let Ok((address, count)) = kernel {
            unsafe { dma_reserve.free(address, count) };
        }
        let dma = dma_reserve.buddy_alloc_below_with_level(
            PageFrameCount::ONE,
            PhysAddr::new(super::super::DMA32_LIMIT),
            ReserveLevel::User,
        );
        dma32_reserve &= dma.is_ok();
        if let Ok((address, count)) = dma {
            unsafe { dma_reserve.free(address, count) };
        }
        dma32_reserve &= dma_reserve.dma32_free_pages == free;
    }
    let reserve = low_reserve && dma32_reserve;

    // A long reported prefix must not hide an unreported tail forever.
    // Synthetic addresses are never dereferenced or submitted to any device;
    // this is the same metadata fixture style as existing buddy selftests.
    let mut scan = unsafe { fresh(base) };
    let source = base + (1024 << MIN_ORDER);
    let zone = BuddyAllocator::<MMArch>::zone_for(source, BLOCK_ORDER as usize);
    let head = scan.free_area(zone, BLOCK_ORDER);
    let mut header: PageList<MMArch> = BuddyAllocator::<MMArch>::read_page(head);
    let prefix = BuddyAllocator::<MMArch>::BUDDY_ENTRIES - 1;
    header.entry_num = prefix + 1;
    header.report_scan = 0;
    let foreign = foreign_base(base);
    scan.free_index
        .relocate(source, BuddyAllocator::<MMArch>::entry_addr(head, prefix));
    for index in 0..prefix {
        let candidate = PhysAddr::new(foreign + (index << BLOCK_ORDER));
        scan.free_index.insert(
            candidate,
            BuddyAllocator::<MMArch>::entry_addr(head, index),
            BLOCK_ORDER,
        );
        unsafe {
            MMArch::write(
                BuddyAllocator::<MMArch>::entry_virt_addr(head, index),
                PhysAddr::new((foreign + (index << BLOCK_ORDER)) | REPORTED),
            );
        }
    }
    unsafe {
        MMArch::write(
            BuddyAllocator::<MMArch>::entry_virt_addr(head, prefix),
            source,
        )
    };
    BuddyAllocator::<MMArch>::write_page(head, header);
    scan.adjust_free_pages(zone, prefix * BLOCK_PAGES, true);
    scan.diagnostics.zones[zone.index()].free_blocks
        [BuddyAllocator::<MMArch>::order2index(BLOCK_ORDER)] += prefix;
    let mut scan_progress = false;
    for _ in 0..16 {
        if let Some(block) = scan.take_reporting_block() {
            scan_progress = block.address() == source;
            scan.finish_reporting_block(block, false);
            break;
        }
    }

    // Fill the recycled metadata head while a real block is held, forcing
    // final putback to consume its exclusively held spare. No heap allocation.
    let mut metadata = unsafe { fresh(base) };
    let mut metadata_lifetime = false;
    if let Some(block) = metadata.take_reporting_block() {
        let address = block.address();
        let zone = BuddyAllocator::<MMArch>::zone_for(address, BLOCK_ORDER as usize);
        let spare = metadata.reporting.spare;
        let foreign = foreign_base(base);
        let mut inserted = 0;
        for index in 0..BuddyAllocator::<MMArch>::BUDDY_ENTRIES + 2 {
            let candidate = PhysAddr::new(foreign + (index << BLOCK_ORDER));
            if candidate == address || candidate.data() == (address.data() ^ (1 << BLOCK_ORDER)) {
                continue;
            }
            unsafe { metadata.insert_without_merge(candidate, BLOCK_ORDER) };
            inserted += 1;
            if inserted == BuddyAllocator::<MMArch>::BUDDY_ENTRIES {
                break;
            }
        }
        metadata.finish_reporting_block(block, true);
        let head = metadata.free_area(zone, BLOCK_ORDER);
        metadata_lifetime = head == spare
            && metadata.reporting.spare.is_null()
            && BuddyAllocator::<MMArch>::read_entry(head, 0) == address
            && metadata.free_entry(address) == Some((BLOCK_ORDER, spare, 0));
    }
    [
        ownership,
        reuse,
        scan_progress,
        metadata_lifetime,
        reserve,
        discard_putback,
    ]
}

// A real placement map must survive converting an individually owned spare
// into high-order reporting metadata. The spare deliberately shares a Movable
// pageblock with live low-order free chains and is not pageblock-aligned.
fn placement_spare(base: PhysAddr) -> bool {
    let managed = base + (1024 << MIN_ORDER);
    let Some(mut buddy) =
        (unsafe { BuddyAllocator::<MMArch>::placement_fixture(base, managed, 2048) })
    else {
        return false;
    };
    // Include the same explicit synthetic identities used to fill metadata;
    // placement labels still describe only the real, managed fixture arena.
    buddy.free_index = reporting_index(base);
    let source = base + (2048 << MIN_ORDER);
    unsafe {
        buddy.buddy_free(source, 21);
    }
    let Ok((pinned, _)) = buddy.buddy_alloc_with_placement(
        PageFrameCount::ONE,
        ReserveLevel::User,
        FramePlacement::Movable,
    ) else {
        return false;
    };
    let Ok((spare, _)) = buddy.buddy_alloc_with_placement(
        PageFrameCount::ONE,
        ReserveLevel::User,
        FramePlacement::Movable,
    ) else {
        return false;
    };
    if spare.check_aligned(1 << 21) {
        return false;
    }
    buddy.reporting.spare = spare;
    unsafe {
        buddy.buddy_free(managed, BLOCK_ORDER);
    }
    let Some(block) = buddy.take_reporting_block() else {
        return false;
    };
    let address = block.address();
    let zone = BuddyAllocator::<MMArch>::zone_for(address, BLOCK_ORDER as usize);
    let label_before = buddy.placement_of(spare);
    let movable_before = buddy.movable_blocks;
    let metadata_before = buddy.metadata_from_movable;
    let free_before = buddy.zone_free(zone);
    let foreign = foreign_base(base);
    let mut inserted = 0;
    for index in 0..BuddyAllocator::<MMArch>::BUDDY_ENTRIES + 2 {
        let candidate = PhysAddr::new(foreign + (index << BLOCK_ORDER));
        if candidate == address || candidate.data() == (address.data() ^ (1 << BLOCK_ORDER)) {
            continue;
        }
        unsafe {
            buddy.insert_without_merge(candidate, BLOCK_ORDER);
        }
        inserted += 1;
        if inserted == BuddyAllocator::<MMArch>::BUDDY_ENTRIES {
            break;
        }
    }
    buddy.finish_reporting_block(block, true);
    let passed = address == managed
        && label_before == FramePlacement::Movable
        && buddy.placement_of(spare) == label_before
        && buddy.placement_of(managed) == FramePlacement::Unmovable
        && buddy.placement_of(managed + (1 << 21)) == FramePlacement::Unmovable
        && buddy.movable_blocks == movable_before
        && buddy.metadata_from_movable == metadata_before + 1
        && buddy.free_area(zone, BLOCK_ORDER) == spare
        && buddy.reporting.spare.is_null()
        && BuddyAllocator::<MMArch>::read_entry(spare, 0) == managed
        && buddy.free_entry(managed) == Some((BLOCK_ORDER, spare, 0))
        && buddy.free_index.lookup(spare).is_none()
        && buddy.zone_free(zone) == free_before + (inserted + 1) * BLOCK_PAGES
        && buddy.diagnostics.zones[zone.index()].free_blocks
            [BuddyAllocator::<MMArch>::order2index(BLOCK_ORDER)]
            == inserted + 1;
    unsafe {
        buddy.free(pinned, PageFrameCount::ONE);
    }
    passed
}

pub(crate) fn report() -> String {
    let result = unsafe { LockedFrameAllocator.allocate(PageFrameCount::new(BACKING)) }
        .map(|(base, _)| {
            let _backing = Backing(base);
            let result = run(base);
            let placement = placement_spare(base);
            [
                result[0], result[1], result[2], result[3], result[4], result[5], placement,
            ]
        })
        .unwrap_or([false; 7]);
    let mut report = String::new();
    let accounting = crate::mm::balloon::accounting_selftest();
    let failures = result.iter().filter(|&&ok| !ok).count() + usize::from(!accounting);
    writeln!(
        report,
        "status={}",
        if failures == 0 { "ok" } else { "fail" }
    )
    .unwrap();
    for (name, ok) in [
        "ownership",
        "reported_reuse",
        "scan_progress",
        "metadata_lifetime",
        "reserve",
        "discard_putback",
        "placement_spare",
    ]
    .into_iter()
    .zip(result)
    {
        writeln!(report, "{name}={}", if ok { "ok" } else { "fail" }).unwrap();
    }
    writeln!(
        report,
        "accounting={}",
        if accounting { "ok" } else { "fail" }
    )
    .unwrap();
    writeln!(report, "summary_fail={failures}").unwrap();
    report
}
