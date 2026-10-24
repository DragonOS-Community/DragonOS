//! Per-attempt DMA allocation evidence. Historical diagnostics remain unchanged.
use super::*;

const CAPACITY_SCAN_BUDGET: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DmaRecoveryZone {
    Normal,
    Dma32,
    /// An unrestricted request admitted into DMA32 above its reserve. Unlike
    /// a hard low-address mask, this contract needs a live handoff check.
    Dma32Fallback,
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
        self.try_allocate_for_dma_at_level(count, max, ReserveLevel::Kernel)
    }

    pub(crate) fn try_allocate_for_dma_at_level(
        &mut self,
        count: PageFrameCount,
        max: PhysAddr,
        level: ReserveLevel,
    ) -> Result<(PhysAddr, PageFrameCount), DmaAllocationFailure> {
        if !count.data().is_power_of_two() || log2(count.data()) >= BUDDY_ORDER_COUNT {
            self.record_failure(None, AllocationFailure::InvalidOrder);
            return Err(DmaAllocationFailure::plain(AllocationFailure::InvalidOrder));
        }
        let result = self.buddy_alloc_below_with_level(count, max, level);
        self.request_pressure_if_needed(result.is_err());
        match result {
            Err(AllocationFailure::AddressLimit) => Err(
                match self.reachable_capacity(count.data(), max, CAPACITY_SCAN_BUDGET) {
                    Some(zone)
                        if self.admits(
                            match zone {
                                DmaRecoveryZone::Normal => BuddyZone::Normal,
                                _ => BuddyZone::Dma32,
                            },
                            count.data(),
                            0,
                            level,
                        ) =>
                    {
                        DmaAllocationFailure::fragmented(zone)
                    }
                    Some(_) => DmaAllocationFailure::plain(AllocationFailure::Reserve),
                    None => DmaAllocationFailure::plain(AllocationFailure::AddressLimit),
                },
            ),
            // A protected low zone does not prevent compacting sufficient
            // Normal free capacity. Historical diagnostics stay unchanged.
            Err(AllocationFailure::Reserve)
                if max.data() == usize::MAX
                    && self.has_normal_memory
                    && self.normal_free_pages >= count.data()
                    && (MIN_ORDER + log2(count.data())..MAX_ORDER).all(|order| {
                        placement::classes(order).iter().all(|&class| {
                            self.class_area(BuddyZone::Normal, order as u8, class)
                                .is_null()
                        })
                    }) =>
            {
                if self.admits(BuddyZone::Normal, count.data(), 0, level) {
                    Err(DmaAllocationFailure::fragmented(DmaRecoveryZone::Normal))
                } else {
                    Err(DmaAllocationFailure::plain(AllocationFailure::Reserve))
                }
            }
            Err(AllocationFailure::Fragmentation) => {
                if max.data() == usize::MAX && self.has_normal_memory {
                    if self.admits(BuddyZone::Normal, count.data(), 0, level) {
                        Err(DmaAllocationFailure::fragmented(DmaRecoveryZone::Normal))
                    } else if self.dma32_fallback_allowed(count.data())
                        && self.admits(BuddyZone::Dma32, count.data(), 0, level)
                    {
                        Err(DmaAllocationFailure::fragmented(
                            DmaRecoveryZone::Dma32Fallback,
                        ))
                    } else {
                        Err(DmaAllocationFailure::plain(AllocationFailure::ZoneLimit))
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
    /// Index reads share one bound across all orders and eligible zones.
    fn reachable_capacity(
        &self,
        needed: usize,
        max: PhysAddr,
        mut budget: usize,
    ) -> Option<DmaRecoveryZone> {
        // The index covers all placement classes. Restrict its read-only census
        // to this mask instead of spending the proof budget on unrelated heads.
        let mask_end = max.data().saturating_add(1) & !(A::PAGE_SIZE - 1);
        for zone in [BuddyZone::Normal, BuddyZone::Dma32] {
            let start = match zone {
                BuddyZone::Normal => DMA32_LIMIT + 1,
                BuddyZone::Dma32 => 0,
            };
            let end = match zone {
                BuddyZone::Normal => mask_end,
                BuddyZone::Dma32 => mask_end.min(DMA32_LIMIT + 1),
            };
            if start >= end {
                continue;
            }
            let mut found = 0usize;
            for order in MIN_ORDER..MAX_ORDER {
                let bytes = 1usize << order;
                let pages = bytes / A::PAGE_SIZE;
                // Head addresses are page aligned: this exclusive bound admits
                // precisely the blocks whose entire extent fits below the mask.
                if end - start >= bytes {
                    let full_end = end - bytes + A::PAGE_SIZE;
                    let heads = self
                        .free_index
                        .count_heads(
                            order as u8,
                            PhysAddr::new(start),
                            PhysAddr::new(full_end),
                            (needed - found).div_ceil(pages),
                            &mut budget,
                        )
                        .ok()?;
                    found = found.saturating_add(heads.saturating_mul(pages));
                }
                if found < needed && !end.is_multiple_of(bytes) {
                    // At most one aligned block at this order straddles the
                    // exclusive mask end. Count only its complete reachable pages.
                    let tail = end & !(bytes - 1);
                    if tail >= start {
                        let present = self
                            .free_index
                            .count_heads(
                                order as u8,
                                PhysAddr::new(tail),
                                PhysAddr::new(tail + A::PAGE_SIZE),
                                1,
                                &mut budget,
                            )
                            .ok()?;
                        found = found.saturating_add(present * ((end - tail) / A::PAGE_SIZE));
                    }
                }
                if found >= needed {
                    return Some(match zone {
                        BuddyZone::Normal => DmaRecoveryZone::Normal,
                        BuddyZone::Dma32 => DmaRecoveryZone::Dma32,
                    });
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
    // Sibling calls keep each fixture's allocator locals out of the other's
    // stack frame. Do not call the fallback case from inside the base case.
    typed_base_allocation_selftest(&fresh)
        && evidence_budget_selftest(&fresh)
        && reserve_domain_selftest(&fresh)
        && floor_classification_selftest(&fresh)
        && finite_zone_proof_selftest(&fresh)
        && fallback_allocation_selftest(&fresh)
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
fn attempt(
    allocator: &mut BuddyAllocator<MMArch>,
    count: PageFrameCount,
    max: PhysAddr,
) -> Result<(PhysAddr, PageFrameCount), AllocationFailure> {
    allocator
        .try_allocate_for_dma(count, max)
        .map_err(|failure| failure.reason)
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
#[inline(never)]
fn typed_base_allocation_selftest(fresh: &impl Fn() -> BuddyAllocator<MMArch>) -> bool {
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

    invalid
        && empty
        && zone
        && max_normalized
        && bounded
        && fragment
        && diagnostic
        && partial_page
        && dma32_fragment
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
#[inline(never)]
fn evidence_budget_selftest(fresh: &impl Fn() -> BuddyAllocator<MMArch>) -> bool {
    evidence_census_selftest(fresh, false)
        && evidence_census_selftest(fresh, true)
        && partial_capacity_selftest(fresh)
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
#[inline(never)]
fn evidence_census_selftest(fresh: &impl Fn() -> BuddyAllocator<MMArch>, masked: bool) -> bool {
    let page = MMArch::PAGE_SIZE;
    let low = PhysAddr::new(0x0200_0000);
    let mut allocator = fresh();
    let metadata = allocator.dma32_empty_area[0];
    let blocks = CAPACITY_SCAN_BUDGET + 1;
    let lists = blocks.div_ceil(BuddyAllocator::<MMArch>::BUDDY_ENTRIES);
    // Keep the useful low-mask heads at the end of a chain containing more
    // entries than the old proof budget. Unrelated free inventory must not
    // prevent proving these eight complete reachable pages.
    let physical = |ordinal: usize| {
        if masked && ordinal < blocks - 8 {
            low + (1 << 21) + ordinal * 2 * page
        } else if masked {
            low + (ordinal - (blocks - 8)) * 2 * page
        } else {
            low + ordinal * 2 * page
        }
    };
    for list_index in 0..lists {
        let address = metadata + (38 + list_index) * page;
        let begin = list_index * BuddyAllocator::<MMArch>::BUDDY_ENTRIES;
        let entries = (blocks - begin).min(BuddyAllocator::<MMArch>::BUDDY_ENTRIES);
        let next = if list_index + 1 == lists {
            PhysAddr::new(0)
        } else {
            address + page
        };
        BuddyAllocator::<MMArch>::write_page(
            address,
            PageList::new(entries, next).with_order(MIN_ORDER as u8),
        );
        for index in 0..entries {
            let head = physical(begin + index);
            unsafe {
                MMArch::write(
                    BuddyAllocator::<MMArch>::entry_virt_addr(address, index),
                    head,
                );
            }
            allocator.free_index.insert(
                head,
                BuddyAllocator::<MMArch>::entry_addr(address, index),
                MIN_ORDER as u8,
            );
        }
    }
    allocator.dma32_free_area[0] = metadata + 38 * page;
    allocator.dma32_free_pages = blocks;
    allocator.diagnostics.zones[0].free_blocks[0] = blocks;
    let needed = if masked { 8 } else { CAPACITY_SCAN_BUDGET };
    let max = if masked {
        low + 16 * page - 1
    } else {
        low + blocks * 2 * page - 1
    };
    let before = allocator.diagnostic_snapshot();
    let first_slot = allocator.free_index.lookup(physical(0));
    let last_slot = allocator.free_index.lookup(physical(blocks - 1));
    // Budget counts actual bounded index work, not the number of free heads.
    // A tiny budget still cannot authorize recovery using a partial proof.
    let proof = allocator.reachable_capacity(needed, max, 1).is_none()
        && allocator.reachable_capacity(needed, max, CAPACITY_SCAN_BUDGET)
            == Some(DmaRecoveryZone::Dma32);
    let typed = allocator.try_allocate_for_dma(PageFrameCount::new(needed), max)
        == Err(DmaAllocationFailure::fragmented(DmaRecoveryZone::Dma32));
    let after = allocator.diagnostic_snapshot();
    proof
        && typed
        && before.free_pages == after.free_pages
        && before.diagnostics.zones[0].free_blocks == after.diagnostics.zones[0].free_blocks
        && allocator.free_index.lookup(physical(0)) == first_slot
        && allocator.free_index.lookup(physical(blocks - 1)) == last_slot
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
#[inline(never)]
fn partial_capacity_selftest(fresh: &impl Fn() -> BuddyAllocator<MMArch>) -> bool {
    let page = MMArch::PAGE_SIZE;
    let low = PhysAddr::new(0x0200_0000);
    let mut allocator = fresh();
    unsafe { allocator.free(low, PageFrameCount::new(16)) };
    let before = allocator.diagnostic_snapshot();
    // The one free block straddles the device limit. An incomplete eighth
    // page cannot provide the eighth page of fragmentation evidence.
    let seven = allocator.reachable_capacity(8, low + 7 * page - 1, CAPACITY_SCAN_BUDGET);
    let short_eighth = allocator.reachable_capacity(8, low + 8 * page - 2, CAPACITY_SCAN_BUDGET);
    let eight = allocator.reachable_capacity(8, low + 8 * page - 1, CAPACITY_SCAN_BUDGET);
    let after = allocator.diagnostic_snapshot();
    seven.is_none()
        && short_eighth.is_none()
        && eight == Some(DmaRecoveryZone::Dma32)
        && before.free_pages == after.free_pages
        && before.diagnostics.zones[0].free_blocks == after.diagnostics.zones[0].free_blocks
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
#[inline(never)]
fn reserve_domain_selftest(fresh: &impl Fn() -> BuddyAllocator<MMArch>) -> bool {
    let page = MMArch::PAGE_SIZE;
    let low = PhysAddr::new(0x0200_0000);
    let high = PhysAddr::new(1usize << 32);
    let eight = PageFrameCount::new(8);

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

    normal && reserve && normal_fragment_despite_reserve && combined_not_domain && finite_cross_zone
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
#[inline(never)]
fn floor_classification_selftest(fresh: &impl Fn() -> BuddyAllocator<MMArch>) -> bool {
    let low = PhysAddr::new(0x0200_0000);
    let high = PhysAddr::new(1usize << 32);
    let eight = PageFrameCount::new(8);

    // Nonzero floors do not turn an undersized placement domain into a
    // reserve denial, and real reserve denials must carry no compaction proof.
    let mut allocator = fresh();
    unsafe {
        allocator.free(high, PageFrameCount::new(4));
        allocator.free(low, PageFrameCount::new(4));
    }
    allocator.watermarks = [ZoneWatermarks {
        min: 1,
        low: 2,
        high: 3,
    }; 2];
    let capacity_with_floor = allocator.try_allocate_for_dma(eight, PhysAddr::new(usize::MAX))
        == Err(DmaAllocationFailure::plain(AllocationFailure::ZoneLimit));
    let mut allocator = fresh();
    unsafe { allocator.free(high, eight) };
    allocator.watermarks[BuddyZone::Normal.index()] = ZoneWatermarks {
        min: 1,
        low: 2,
        high: 3,
    };
    let reserve_with_capacity = allocator.try_allocate_for_dma(eight, PhysAddr::new(usize::MAX))
        == Err(DmaAllocationFailure::plain(AllocationFailure::Reserve))
        && allocator.normal_free_pages == eight.data();

    capacity_with_floor && reserve_with_capacity
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
#[inline(never)]
fn finite_zone_proof_selftest(fresh: &impl Fn() -> BuddyAllocator<MMArch>) -> bool {
    let page = MMArch::PAGE_SIZE;
    let low = PhysAddr::new(0x0200_0000);
    let high = PhysAddr::new(1usize << 32);
    let eight = PageFrameCount::new(8);
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

    finite_low_proof && finite_normal_proof
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
#[inline(never)]
fn fallback_allocation_selftest(fresh: &impl Fn() -> BuddyAllocator<MMArch>) -> bool {
    let page = MMArch::PAGE_SIZE;
    let low = PhysAddr::new(0x0200_0000);
    let high = PhysAddr::new(1usize << 32);
    // A wide request may use DMA32 above its protected reserve when Normal
    // lacks capacity. Fragmentation in that admitted fallback zone is still
    // recoverable; combined-zone counters must not erase its identity.
    let mut allocator = fresh();
    unsafe { allocator.free(high, PageFrameCount::ONE) };
    for index in 0..19 {
        unsafe {
            allocator.free(low + index * 512 * page, PageFrameCount::new(256));
        }
    }
    let fallback =
        allocator.try_allocate_for_dma(PageFrameCount::new(512), PhysAddr::new(usize::MAX));
    let eligible_low_fallback = fallback
        == Err(DmaAllocationFailure::fragmented(
            DmaRecoveryZone::Dma32Fallback,
        ))
        && allocator.diagnostics.failure_reasons[AllocationFailure::Fragmentation as usize] == 1
        && allocator.diagnostics.zones[BuddyZone::Normal.index()].attempts[9] == 1
        && allocator.diagnostics.zones[BuddyZone::Dma32.index()].attempts[9] == 1
        && allocator.dma32_free_pages == 19 * 256
        && allocator.normal_free_pages == 1;

    // Model the live counter after a captured run is no longer on free lists.
    // A competing constrained consumer can move it across this boundary.
    let mut live = fresh();
    let no_normal_unrestricted = live.dma32_fallback_allowed(512);
    unsafe { live.free(high, PageFrameCount::ONE) };
    let floor = DMA32_FALLBACK_RESERVE_PAGES + DMA32_ALLOCATION_METADATA_HEADROOM;
    live.dma32_free_pages = floor - 1;
    let below_floor = !live.dma32_fallback_allowed(0);
    live.dma32_free_pages = floor;
    let at_floor = live.dma32_fallback_allowed(0) && !live.dma32_fallback_allowed(1);
    live.dma32_free_pages = floor + 1;
    let additional_page = live.dma32_fallback_allowed(1);
    let live_reserve_boundary =
        no_normal_unrestricted && below_floor && at_floor && additional_page;

    eligible_low_fallback && live_reserve_boundary
}
