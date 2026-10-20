//! Per-attempt DMA allocation evidence. Historical diagnostics remain unchanged.
use super::*;

const CAPACITY_SCAN_BUDGET: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DmaRecoveryZone {
    Normal,
    Dma32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DmaAllocationFailure {
    pub reason: AllocationFailure,
    pub zone: Option<DmaRecoveryZone>,
}

impl DmaAllocationFailure {
    fn plain(reason: AllocationFailure) -> Self {
        Self { reason, zone: None }
    }

    fn fragmented(zone: DmaRecoveryZone) -> Self {
        Self {
            reason: AllocationFailure::Fragmentation,
            zone: Some(zone),
        }
    }
}

impl<A: MemoryManagementArch> BuddyAllocator<A> {
    /// One ordinary buddy attempt, without reclaim, followed only on a
    /// constrained miss by bounded evidence gathering. No cumulative counters
    /// are used to infer the reason for this request.
    pub(crate) fn try_allocate_for_dma(
        &mut self,
        count: PageFrameCount,
        max: PhysAddr,
    ) -> Result<(PhysAddr, PageFrameCount), DmaAllocationFailure> {
        if !count.data().is_power_of_two() || log2(count.data()) >= BUDDY_ORDER_COUNT {
            self.record_failure(None, AllocationFailure::InvalidOrder);
            return Err(DmaAllocationFailure::plain(AllocationFailure::InvalidOrder));
        }
        match self.buddy_alloc_below_result(count, max) {
            Err(AllocationFailure::AddressLimit) => Err(self
                .reachable_capacity(count.data(), max, CAPACITY_SCAN_BUDGET)
                .map(DmaAllocationFailure::fragmented)
                .unwrap_or_else(|| DmaAllocationFailure::plain(AllocationFailure::AddressLimit))),
            // A protected low zone does not prevent compacting sufficient
            // Normal free capacity. Historical diagnostics stay unchanged.
            Err(AllocationFailure::Reserve)
                if max.data() == usize::MAX
                    && self.has_normal_memory
                    && self.normal_free_pages >= count.data() =>
            {
                Err(DmaAllocationFailure::fragmented(DmaRecoveryZone::Normal))
            }
            Err(AllocationFailure::Fragmentation) => {
                if max.data() == usize::MAX && self.has_normal_memory {
                    if self.normal_free_pages < count.data() {
                        Err(DmaAllocationFailure::plain(AllocationFailure::ZoneLimit))
                    } else {
                        Err(DmaAllocationFailure::fragmented(DmaRecoveryZone::Normal))
                    }
                } else {
                    Err(DmaAllocationFailure::fragmented(DmaRecoveryZone::Dma32))
                }
            }
            Err(reason) => Err(DmaAllocationFailure::plain(reason)),
            Ok(allocation) => Ok(allocation),
        }
    }

    /// Count complete base pages, not bytes or zone totals, wholly below the
    /// inclusive DMA limit. Stop as soon as sufficient evidence exists.
    /// Metadata visits share the bound so even empty chains cannot run free.
    fn reachable_capacity(
        &self,
        needed: usize,
        max: PhysAddr,
        mut budget: usize,
    ) -> Option<DmaRecoveryZone> {
        for zone in [BuddyZone::Normal, BuddyZone::Dma32] {
            if zone == BuddyZone::Normal && max.data() <= DMA32_LIMIT {
                continue;
            }
            let mut found = 0usize;
            for order in MIN_ORDER..MAX_ORDER {
                let mut address = self.free_area(zone, order as u8);
                while !address.is_null() {
                    if budget == 0 {
                        return None;
                    }
                    budget -= 1;
                    let list: PageList<A> = Self::read_page(address);
                    for index in 0..list.entry_num {
                        if budget == 0 {
                            return None;
                        }
                        budget -= 1;
                        let base: PhysAddr =
                            unsafe { A::read(Self::entry_virt_addr(address, index)) };
                        if let Some(last_offset) = max.data().checked_sub(base.data()) {
                            // max is finite here, hence this saturating add
                            // also safely handles a maximal suffix in tests.
                            let bytes = last_offset.saturating_add(1).min(1usize << order);
                            found += bytes / A::PAGE_SIZE;
                            if found >= needed {
                                return Some(match zone {
                                    BuddyZone::Normal => DmaRecoveryZone::Normal,
                                    BuddyZone::Dma32 => DmaRecoveryZone::Dma32,
                                });
                            }
                        }
                    }
                    address = list.next_page;
                }
            }
        }
        None
    }
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
pub(super) fn typed_allocation_selftest(fresh: impl Fn() -> BuddyAllocator<MMArch>) -> bool {
    fn attempt(
        allocator: &mut BuddyAllocator<MMArch>,
        count: PageFrameCount,
        max: PhysAddr,
    ) -> Result<(PhysAddr, PageFrameCount), AllocationFailure> {
        allocator
            .try_allocate_for_dma(count, max)
            .map_err(|failure| failure.reason)
    }
    let page = MMArch::PAGE_SIZE;
    let low = PhysAddr::new(0x0200_0000);
    let high = PhysAddr::new(1usize << 32);
    let eight = PageFrameCount::new(8);
    let mut allocator = fresh();
    let invalid = attempt(
        &mut allocator,
        PageFrameCount::new(0),
        PhysAddr::new(usize::MAX),
    ) == Err(AllocationFailure::InvalidOrder);
    let empty = attempt(&mut allocator, eight, PhysAddr::new(DMA32_LIMIT))
        == Err(AllocationFailure::Capacity);
    unsafe { allocator.free(high, eight) };
    let zone = allocator.has_normal_memory
        && attempt(&mut allocator, eight, PhysAddr::new(DMA32_LIMIT))
            == Err(AllocationFailure::ZoneLimit);
    let max_normalized =
        attempt(&mut allocator, eight, PhysAddr::new(usize::MAX)) == Ok((high, eight));

    let mut allocator = fresh();
    for index in 0..8 {
        unsafe { allocator.free(low + 2 * index * page, PageFrameCount::ONE) };
    }
    let mask = low + 16 * page - 1;
    let before = allocator.diagnostic_snapshot();
    let bounded = allocator.reachable_capacity(8, mask, 1).is_none()
        && allocator.reachable_capacity(8, mask, CAPACITY_SCAN_BUDGET)
            == Some(DmaRecoveryZone::Dma32);
    let fragment = allocator.try_allocate_for_dma(eight, mask)
        == Err(DmaAllocationFailure::fragmented(DmaRecoveryZone::Dma32));
    // The old diagnostic class is deliberately independent of the additional
    // typed capacity proof, and the evidence pass does not mutate ownership.
    let after = allocator.diagnostic_snapshot();
    let diagnostic = after.free_pages == before.free_pages
        && after.diagnostics.failure_reasons[AllocationFailure::AddressLimit as usize] == 1
        && after.diagnostics.failure_reasons[AllocationFailure::Fragmentation as usize] == 0;
    let partial_page = attempt(&mut allocator, eight, low + 14 * page + page - 2)
        == Err(AllocationFailure::AddressLimit);
    let dma32_fragment = attempt(&mut allocator, eight, PhysAddr::new(DMA32_LIMIT))
        == Err(AllocationFailure::Fragmentation);

    // More entries than the evidence budget: ordinary allocation sees no
    // high-order blocks, but must not label the partial capacity scan as
    // proven fragmentation. Metadata uses real private-fixture RAM; the
    // listed frame identities are never dereferenced or allocated here.
    let mut budget_allocator = fresh();
    let metadata = budget_allocator.dma32_empty_area[0];
    let blocks = CAPACITY_SCAN_BUDGET + 1;
    let lists = blocks.div_ceil(BuddyAllocator::<MMArch>::BUDDY_ENTRIES);
    for list_index in 0..lists {
        let address = metadata + (38 + list_index) * page;
        let begin = list_index * BuddyAllocator::<MMArch>::BUDDY_ENTRIES;
        let entries = (blocks - begin).min(BuddyAllocator::<MMArch>::BUDDY_ENTRIES);
        let next = if list_index + 1 == lists {
            PhysAddr::new(0)
        } else {
            address + page
        };
        BuddyAllocator::<MMArch>::write_page(address, PageList::new(entries, next));
        for index in 0..entries {
            unsafe {
                MMArch::write(
                    BuddyAllocator::<MMArch>::entry_virt_addr(address, index),
                    low + (begin + index) * 2 * page,
                );
            }
        }
    }
    budget_allocator.dma32_free_area[0] = metadata + 38 * page;
    budget_allocator.dma32_free_pages = blocks;
    budget_allocator.diagnostics.zones[0].free_blocks[0] = blocks;
    let evidence_exhausted = attempt(
        &mut budget_allocator,
        PageFrameCount::new(CAPACITY_SCAN_BUDGET),
        low + blocks * 2 * page - 1,
    ) == Err(AllocationFailure::AddressLimit);

    let mut allocator = fresh();
    unsafe {
        allocator.free(low, PageFrameCount::new(DMA32_FALLBACK_RESERVE_PAGES));
        allocator.free(high, PageFrameCount::ONE);
    }
    let normal = attempt(
        &mut allocator,
        PageFrameCount::ONE,
        PhysAddr::new(usize::MAX),
    ) == Ok((high, PageFrameCount::ONE));
    let reserve = attempt(&mut allocator, eight, PhysAddr::new(usize::MAX))
        == Err(AllocationFailure::Reserve);

    let mut allocator = fresh();
    unsafe { allocator.free(low, PageFrameCount::new(DMA32_FALLBACK_RESERVE_PAGES)) };
    for index in 0..8 {
        unsafe { allocator.free(high + index * 2 * page, PageFrameCount::ONE) };
    }
    let normal_fragment_despite_reserve = allocator
        .try_allocate_for_dma(eight, PhysAddr::new(usize::MAX))
        == Err(DmaAllocationFailure::fragmented(DmaRecoveryZone::Normal))
        && allocator.diagnostics.failure_reasons[AllocationFailure::Reserve as usize] == 1
        && allocator.diagnostics.failure_reasons[AllocationFailure::Fragmentation as usize] == 0
        && allocator.normal_free_pages == 8
        && allocator.dma32_free_pages == DMA32_FALLBACK_RESERVE_PAGES;

    let mut allocator = fresh();
    unsafe {
        allocator.free(high, PageFrameCount::new(4));
        allocator.free(low, PageFrameCount::new(4));
    }
    let combined_not_domain = attempt(&mut allocator, eight, PhysAddr::new(usize::MAX))
        == Err(AllocationFailure::ZoneLimit);
    let finite_cross_zone =
        attempt(&mut allocator, eight, high + 4 * page - 1) == Err(AllocationFailure::AddressLimit);
    // Preserve which finite-mask zone supplied the proof. A candidate in the
    // other zone would be capacity evacuation, not pure fragmentation recovery.
    let mut allocator = fresh();
    for index in 0..8 {
        unsafe { allocator.free(low + index * 2 * page, PageFrameCount::ONE) };
    }
    unsafe { allocator.free(high, PageFrameCount::new(4)) };
    let finite_low_proof = allocator.try_allocate_for_dma(eight, high + 4 * page - 1)
        == Err(DmaAllocationFailure::fragmented(DmaRecoveryZone::Dma32));

    let mut allocator = fresh();
    unsafe { allocator.free(low, PageFrameCount::new(4)) };
    for index in 0..8 {
        unsafe { allocator.free(high + index * 2 * page, PageFrameCount::ONE) };
    }
    let finite_normal_proof = allocator.try_allocate_for_dma(eight, high + 16 * page - 1)
        == Err(DmaAllocationFailure::fragmented(DmaRecoveryZone::Normal));

    invalid
        && empty
        && zone
        && max_normalized
        && bounded
        && fragment
        && diagnostic
        && evidence_exhausted
        && partial_page
        && dma32_fragment
        && normal
        && reserve
        && normal_fragment_despite_reserve
        && combined_not_domain
        && finite_cross_zone
        && finite_low_proof
        && finite_normal_proof
}
