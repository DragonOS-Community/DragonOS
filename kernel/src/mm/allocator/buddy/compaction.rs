//! A single range owner keeps reclaimed source frames out of normal free lists.
//! This module knows only frame ownership. Page eligibility, migration, request
//! backoff and placement policy belong to the caller outside the buddy lock.

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
use super::PageList;
use super::{placement, BuddyAllocator, BuddyZone, FramePlacement, MAX_ORDER, MIN_ORDER};
use crate::mm::{MemoryManagementArch, PhysAddr};
#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
use alloc::vec::Vec;

pub(crate) const MAX_COMPACT_ORDER: usize = 9;
const _: () = assert!(MAX_COMPACT_ORDER + MIN_ORDER <= placement::BLOCK_ORDER);
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
            let (low, high) = match zone {
                BuddyZone::Dma32 => (start.data(), end.data().min(1usize << 32)),
                BuddyZone::Normal => (start.data().max(1usize << 32), end.data()),
            };
            if low >= high || high - low < bytes {
                continue;
            }
            for current in MIN_ORDER + order..MAX_ORDER {
                // The head of a larger containing block can precede the
                // request. Otherwise the index skips unrelated free blocks,
                // including all lower orders, via nonempty-word summaries.
                let ancestor = PhysAddr::new(low & !((1usize << current) - 1));
                charge(remaining_scan, self.index_lookup_bound())?;
                let ancestor_selected =
                    low.checked_add(bytes - 1).map(|value| value & !(bytes - 1));
                let ancestor_end = ancestor.data().checked_add(1usize << current);
                let usable_ancestor = self.free_entry(ancestor).is_some_and(|(found, _, _)| {
                    found as usize == current
                        && ancestor_selected.is_some_and(|selected| {
                            selected.checked_add(bytes).is_some_and(|last| {
                                ancestor_end.is_some_and(|block_end| last <= block_end.min(high))
                            })
                        })
                });
                let base = if usable_ancestor {
                    Some(ancestor)
                } else {
                    self.index_next_head(
                        current as u8,
                        PhysAddr::new(low),
                        PhysAddr::new(high),
                        remaining_scan,
                    )?
                };
                let Some(base) = base else { continue };
                let block_end = base
                    .data()
                    .checked_add(1usize << current)
                    .ok_or(RangeError::Invalid)?;
                let selected = low
                    .max(base.data())
                    .checked_add(bytes - 1)
                    .map(|value| value & !(bytes - 1))
                    .ok_or(RangeError::Invalid)?;
                if selected
                    .checked_add(bytes)
                    .is_none_or(|limit| limit > high.min(block_end))
                {
                    continue;
                }
                let (found, metadata, index) = self.free_entry(base).expect("indexed head missing");
                assert_eq!(found as usize, current);
                let class = if current < placement::BLOCK_ORDER {
                    self.placement.get::<A>(base)
                } else {
                    FramePlacement::Unmovable
                };
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
                let mutations =
                    current - (MIN_ORDER + order) + 1 + headroom * (MAX_ORDER - MIN_ORDER);
                charge(
                    remaining_scan,
                    self.compaction_point_bound(mutations)
                        .ok_or(RangeError::Invalid)?,
                )?;
                let removed = self.remove_entry(zone, current as u8, class, metadata, index);
                assert_eq!(removed, base);
                let selected = PhysAddr::new(selected);
                self.classify_allocation(
                    selected,
                    current as u8,
                    (MIN_ORDER + order) as u8,
                    FramePlacement::Unmovable,
                );
                unsafe { self.return_outside_halves(base, current, selected, MIN_ORDER + order) };
                return Ok(Some(selected));
            }
        }
        Ok(None)
    }

    /// Local heads, their slot replacements and order-bounded splits are
    /// prepaid before mutation. Unrelated zone inventory is never charged.
    fn isolation_work_bound(
        &self,
        zone: BuddyZone,
        target_order: usize,
        start: PhysAddr,
    ) -> Option<usize> {
        let pages = 1usize << (target_order - MIN_ORDER);
        let ancestors = MAX_ORDER - target_order;
        // Both the read-only preflight and removal's ancestor lookup are
        // charged. Their bound is independent of the free-list population.
        let probes = ancestors
            .checked_mul(2)?
            .checked_mul(self.index_lookup_bound())?;
        for current in target_order + 1..MAX_ORDER {
            let base = PhysAddr::new(start.data() & !((1usize << current) - 1));
            if self
                .free_entry(base)
                .is_some_and(|(found, _, _)| found as usize == current)
            {
                let class = if current < placement::BLOCK_ORDER {
                    self.placement.get::<A>(base)
                } else {
                    FramePlacement::Unmovable
                };
                let headroom =
                    self.split_headroom_for(zone, target_order as u8, current as u8, class);
                let mutations = (current - target_order + 1)
                    .checked_add(headroom.checked_mul(MAX_ORDER - MIN_ORDER)?)?;
                return probes.checked_add(self.compaction_point_bound(mutations)?);
            }
        }
        probes.checked_add(self.index_isolation_bound(start, pages)?)
    }

    /// Prepay a conservative bound, then remove intersecting heads by address
    /// lookup under the same buddy lock. Budget failure cannot change ownership.
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
            .isolation_work_bound(zone, MIN_ORDER + order, start)
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
        // Every supported aligned target is at most one placement pageblock.
        // Its low-order heads therefore share a label, which remains stable
        // while the buddy lock is held. Query once, rather than per free head.
        let target_class = self.placement.get::<A>(start);
        // First inspect the aligned ancestors: a larger free block may
        // contain start even though its head lies outside the target.
        for current in (MIN_ORDER + order + 1..MAX_ORDER).rev() {
            let base = PhysAddr::new(start.data() & !((1usize << current) - 1));
            if let Some((found, metadata, index)) = self.free_entry(base) {
                if found as usize != current {
                    continue;
                }
                let class = if current < placement::BLOCK_ORDER {
                    target_class
                } else {
                    FramePlacement::Unmovable
                };
                assert_eq!(self.remove_entry(zone, found, class, metadata, index), base);
                containing = Some((base, current));
                state.mark_free(start, pages);
                break;
            }
        }
        if containing.is_none() {
            // No insertion until all overlapping heads are gone. Deleting
            // an entry can move any other head's slot; query the index anew
            // instead of retaining PageList iterators across replacements.
            let mut address = start.data();
            while address < end {
                let base = PhysAddr::new(address);
                if let Some((found, metadata, index)) = self.free_entry(base) {
                    let current = found as usize;
                    assert!(
                        current <= MIN_ORDER + order,
                        "overlapping buddy free blocks"
                    );
                    assert!(blocks < pages, "too many target free blocks");
                    blocks += 1;
                    let class = if current < placement::BLOCK_ORDER {
                        target_class
                    } else {
                        FramePlacement::Unmovable
                    };
                    assert_eq!(self.remove_entry(zone, found, class, metadata, index), base);
                    state.mark_free(base, 1usize << (current - MIN_ORDER));
                    address += 1usize << current;
                } else {
                    address += 1usize << MIN_ORDER;
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
    let mut checks = selftest::checks();
    checks.extend(super::free_index::selftest_checks());
    checks
}
