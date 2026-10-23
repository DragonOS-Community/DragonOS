//! A single range owner keeps reclaimed source frames out of normal free lists.
//! This module knows only frame ownership. Page eligibility, migration, request
//! backoff and placement policy belong to the caller outside the buddy lock.

use super::{placement, BuddyAllocator, BuddyZone, FramePlacement, PageList, MAX_ORDER, MIN_ORDER};
use crate::mm::{MemoryManagementArch, PhysAddr};
#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
use alloc::vec::Vec;

pub(crate) const MAX_COMPACT_ORDER: usize = 9;
const BITMAP_WORDS: usize = (1 << MAX_COMPACT_ORDER) / u64::BITS as usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RangeError {
    Invalid,
    Busy,
    Budget,
}

/// A non-cloneable identity, not an independently releasable copy of a range.
#[derive(Debug)]
pub(crate) struct IsolationKey {
    id: u64,
    start: PhysAddr,
    pages: usize,
}

impl IsolationKey {
    pub(crate) fn start(&self) -> PhysAddr {
        self.start
    }
    pub(crate) fn pages(&self) -> usize {
        self.pages
    }
}

#[derive(Debug)]
pub(super) struct IsolationState {
    id: u64,
    start: PhysAddr,
    pages: usize,
    free: [u64; BITMAP_WORDS],
    free_pages: usize,
}

impl IsolationState {
    fn matches(&self, key: &IsolationKey) -> bool {
        self.id == key.id && self.start == key.start && self.pages == key.pages
    }

    fn end(&self) -> usize {
        self.start.data() + (self.pages << MIN_ORDER)
    }

    fn contains(&self, address: PhysAddr) -> bool {
        address.data() >= self.start.data() && address.data() < self.end()
    }

    fn is_free(&self, index: usize) -> bool {
        self.free[index / 64] & (1u64 << (index % 64)) != 0
    }

    fn mark_free(&mut self, base: PhysAddr, pages: usize) {
        let first = (base.data() - self.start.data()) >> MIN_ORDER;
        assert!(first + pages <= self.pages);
        for index in first..first + pages {
            let word = &mut self.free[index / 64];
            let bit = 1u64 << (index % 64);
            assert_eq!(*word & bit, 0, "isolated frame freed twice");
            *word |= bit;
        }
        self.free_pages += pages;
    }
}

fn request_pages(order: usize) -> Result<usize, RangeError> {
    if !(1..=MAX_COMPACT_ORDER).contains(&order) {
        return Err(RangeError::Invalid);
    }
    Ok(1 << order)
}

fn charge(remaining: &mut usize, work: usize) -> Result<(), RangeError> {
    if *remaining < work {
        *remaining = 0;
        return Err(RangeError::Budget);
    }
    *remaining -= work;
    Ok(())
}

fn checked_end(start: PhysAddr, pages: usize) -> Result<usize, RangeError> {
    start
        .data()
        .checked_add(pages << MIN_ORDER)
        .ok_or(RangeError::Invalid)
}

impl<A: MemoryManagementArch> BuddyAllocator<A> {
    /// Allocate a complete aligned sub-block within the exclusive search range.
    /// Scanning and its failure consume the caller's shared request budget.
    pub(crate) fn allocate_in_range(
        &mut self,
        order: usize,
        start: PhysAddr,
        end: PhysAddr,
        remaining_scan: &mut usize,
    ) -> Result<Option<PhysAddr>, RangeError> {
        let pages = request_pages(order)?;
        let bytes = pages << MIN_ORDER;
        if start >= end
            || !start.check_aligned(1 << MIN_ORDER)
            || !end.check_aligned(1 << MIN_ORDER)
            || end.data() - start.data() < bytes
        {
            return Err(RangeError::Invalid);
        }
        // This is a hard address-domain request, like allocate_below with a
        // finite limit. It does not use unrestricted-allocation DMA32 fallback.
        for zone in [BuddyZone::Normal, BuddyZone::Dma32] {
            for current in MIN_ORDER + order..MAX_ORDER {
                for &class in placement::classes(current) {
                    charge(remaining_scan, 1)?;
                    let mut metadata = self.class_area(zone, current as u8, class);
                    while !metadata.is_null() {
                        charge(remaining_scan, 1)?;
                        let list: PageList<A> = Self::read_page(metadata);
                        for index in (0..list.entry_num).rev() {
                            charge(remaining_scan, 1)?;
                            let base: PhysAddr =
                                unsafe { A::read(Self::entry_virt_addr(metadata, index)) };
                            let block_end = base
                                .data()
                                .checked_add(1 << current)
                                .ok_or(RangeError::Invalid)?;
                            let low = base.data().max(start.data());
                            let Some(selected) =
                                low.checked_add(bytes - 1).map(|value| value & !(bytes - 1))
                            else {
                                continue;
                            };
                            if selected
                                .checked_add(bytes)
                                .is_none_or(|limit| limit > block_end.min(end.data()))
                            {
                                continue;
                            }
                            // Every following action has fixed order-bounded work;
                            // reserve its split iterations before removing ownership.
                            charge(remaining_scan, current - (MIN_ORDER + order) + 1)?;
                            let split_class = if current >= placement::BLOCK_ORDER {
                                FramePlacement::Unmovable
                            } else {
                                class
                            };
                            let headroom = self.split_headroom_for(
                                zone,
                                (MIN_ORDER + order) as u8,
                                current as u8,
                                split_class,
                            );
                            charge(remaining_scan, headroom * (MAX_ORDER - MIN_ORDER))?;
                            let removed =
                                self.remove_entry(zone, current as u8, class, metadata, index);
                            debug_assert_eq!(removed, base);
                            let selected = PhysAddr::new(selected);
                            self.classify_allocation(
                                selected,
                                current as u8,
                                (MIN_ORDER + order) as u8,
                                FramePlacement::Unmovable,
                            );
                            unsafe {
                                self.return_outside_halves(
                                    base,
                                    current,
                                    selected,
                                    MIN_ORDER + order,
                                )
                            };
                            return Ok(Some(selected));
                        }
                        metadata = list.next_page;
                    }
                }
            }
        }
        Ok(None)
    }

    /// Reserve one complete scan before changing ownership. Non-head metadata
    /// pages contain at least BUDDY_ENTRIES - 1 entries: a new head replaces a
    /// full head, possibly after consuming one old entry for its own metadata.
    /// Removal replaces non-head slots from the head, preserving their density.
    /// Thus the exact free-block counters also bound metadata visits without
    /// walking the chains just to decide whether the scan is affordable.
    fn isolation_work_bound(&self, zone: BuddyZone, target_order: usize) -> Option<usize> {
        let mut work = 0usize;
        for order in MIN_ORDER..MAX_ORDER {
            for &class in placement::classes(order) {
                let blocks = self.class_blocks(zone, order, class);
                work = work
                    .checked_add(1)?
                    .checked_add(blocks)?
                    .checked_add(blocks.div_ceil(Self::BUDDY_ENTRIES - 1))?;
            }
        }
        // At most one free block per target page is removed. A containing
        // larger block needs at most this many outside-half insertions.
        work.checked_add(1 << (target_order - MIN_ORDER))?
            .checked_add(MAX_ORDER - 1 - target_order)?
            .checked_add(placement::MAX_METADATA_HEADROOM * (MAX_ORDER - MIN_ORDER))
    }

    /// Prepay a conservative bound, then remove intersecting blocks in one
    /// pass under the same buddy lock. Budget failure cannot change ownership.
    pub(crate) fn begin_isolation(
        &mut self,
        start: PhysAddr,
        order: usize,
        remaining_scan: &mut usize,
    ) -> Result<IsolationKey, RangeError> {
        let pages = request_pages(order)?;
        let end = checked_end(start, pages)?;
        if !start.check_aligned(pages << MIN_ORDER) {
            return Err(RangeError::Invalid);
        }
        let zone = Self::zone_for(start, MIN_ORDER + order);
        if self.isolation.is_some() {
            return Err(RangeError::Busy);
        }
        let next_id = self
            .next_isolation_id
            .checked_add(1)
            .ok_or(RangeError::Invalid)?;
        let work = self
            .isolation_work_bound(zone, MIN_ORDER + order)
            .ok_or(RangeError::Invalid)?;
        charge(remaining_scan, work)?;
        // The reservation is deliberately conservative and is not refunded.
        // From here onward all fallible request checks are complete. Free-list
        // corruption is an allocator invariant violation, not a recoverable
        // error which could abandon frames already removed into local state.
        let mut blocks = 0;
        let mut containing = None;
        let mut state = IsolationState {
            id: self.next_isolation_id,
            start,
            pages,
            free: [0; BITMAP_WORDS],
            free_pages: 0,
        };
        // No insertion is allowed until every target block is removed. A
        // head-last replacement comes from an already scanned position and
        // is therefore outside the target. Descending indices stay valid.
        for current in MIN_ORDER..MAX_ORDER {
            for &class in placement::classes(current) {
                let mut metadata = self.class_area(zone, current as u8, class);
                while !metadata.is_null() {
                    let list: PageList<A> = Self::read_page(metadata);
                    // remove_entry may recycle this page into the empty chain.
                    // Keep its original successor before that overwrites next.
                    let next = list.next_page;
                    for index in (0..list.entry_num).rev() {
                        let base: PhysAddr =
                            unsafe { A::read(Self::entry_virt_addr(metadata, index)) };
                        let block_end = base
                            .data()
                            .checked_add(1 << current)
                            .expect("buddy free block address overflow");
                        if base.data() < end && block_end > start.data() {
                            assert!(blocks < pages, "overlapping buddy free blocks");
                            blocks += 1;
                            // Aligned power-of-two intervals are disjoint or
                            // nested. Only one larger block can contain a target.
                            if current > MIN_ORDER + order {
                                assert!(
                                    containing.is_none() && base <= start && block_end >= end,
                                    "invalid containing buddy free block"
                                );
                                containing = Some((base, current));
                            }
                            let removed =
                                self.remove_entry(zone, current as u8, class, metadata, index);
                            assert_eq!(removed, base, "isolation removal slot changed");
                            let first = start.data().max(base.data());
                            let last = end.min(block_end);
                            state.mark_free(PhysAddr::new(first), (last - first) >> MIN_ORDER);
                        }
                    }
                    metadata = next;
                }
            }
        }
        self.next_isolation_id = next_id;
        self.isolation = Some(state);
        // Only after the target is absent from every list can insertion's
        // metadata allocator safely consume free outside siblings.
        if let Some((base, current)) = containing {
            unsafe { self.return_outside_halves(base, current, start, MIN_ORDER + order) };
        }
        Ok(IsolationKey {
            id: next_id - 1,
            start,
            pages,
        })
    }

    pub(crate) fn isolated_free(&self, key: &IsolationKey, address: PhysAddr) -> bool {
        self.isolation.as_ref().is_some_and(|state| {
            state.matches(key)
                && state.contains(address)
                && address.check_aligned(1 << MIN_ORDER)
                && state.is_free((address.data() - state.start.data()) >> MIN_ORDER)
        })
    }

    pub(crate) fn isolated_free_pages(&self, key: &IsolationKey) -> usize {
        self.isolation
            .as_ref()
            .filter(|state| state.matches(key))
            .map_or(0, |state| state.free_pages)
    }

    pub(crate) fn capture_isolation(&mut self, key: &IsolationKey) -> Option<PhysAddr> {
        let state = self.isolation.as_ref()?;
        if !state.matches(key) || state.free_pages != state.pages {
            return None;
        }
        let start = state.start;
        if state.pages == (1 << (placement::BLOCK_ORDER - MIN_ORDER)) {
            self.placement
                .set::<A>(start, placement::BLOCK_ORDER, FramePlacement::Unmovable);
        }
        Some(self.isolation.take().unwrap().start)
    }

    pub(crate) fn release_isolation(&mut self, key: &IsolationKey) {
        if !self
            .isolation
            .as_ref()
            .is_some_and(|state| state.matches(key))
        {
            return;
        }
        let state = self.isolation.take().unwrap();
        let mut index = 0;
        while index < state.pages {
            if !state.is_free(index) {
                index += 1;
                continue;
            }
            let mut order = 0;
            while order < MAX_COMPACT_ORDER {
                let next_pages = 1 << (order + 1);
                if index % next_pages != 0
                    || index + next_pages > state.pages
                    || (index..index + next_pages).any(|candidate| !state.is_free(candidate))
                {
                    break;
                }
                order += 1;
            }
            unsafe {
                self.buddy_free(
                    state.start + (index << MIN_ORDER),
                    (MIN_ORDER + order) as u8,
                )
            };
            index += 1 << order;
        }
    }

    /// The central free path calls this before any merging or metadata use.
    pub(super) fn quarantine_free(&mut self, base: PhysAddr, order: u8) -> bool {
        let Some(state) = self.isolation.as_ref() else {
            return false;
        };
        let end = base
            .data()
            .checked_add(1usize << order)
            .expect("buddy free address overflow");
        if base.data() >= state.end() || end <= state.start.data() {
            return false;
        }
        if base.data() >= state.start.data() && end <= state.end() {
            self.isolation
                .as_mut()
                .unwrap()
                .mark_free(base, 1 << (order as usize - MIN_ORDER));
        } else {
            assert!(order as usize > MIN_ORDER);
            let half_order = order - 1;
            unsafe {
                self.buddy_free(base, half_order);
                self.buddy_free(base + (1usize << half_order), half_order);
            }
        }
        true
    }

    /// Return the siblings along the path to a selected aligned sub-block.
    /// The selected block remains owned and never reaches a metadata list.
    unsafe fn return_outside_halves(
        &mut self,
        mut base: PhysAddr,
        mut order: usize,
        selected: PhysAddr,
        requested: usize,
    ) {
        while order > requested {
            order -= 1;
            let middle = base + (1 << order);
            if selected < middle {
                self.insert_without_merge(middle, order as u8);
            } else {
                self.insert_without_merge(base, order as u8);
                base = middle;
            }
        }
        debug_assert_eq!(base, selected);
    }
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
mod selftest;

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
pub(crate) fn selftest_checks() -> Vec<(&'static str, bool)> {
    selftest::checks()
}
