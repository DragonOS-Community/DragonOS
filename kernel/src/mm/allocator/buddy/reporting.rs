//! Free-page reporting owns a single removed buddy block until device completion.
//! Metadata pages are permanent; a separate ring permits bounded, resumable scans
//! without retaining pointers into the mutable active/empty chains.
use super::super::{page_frame::PageFrameCount, reserve::ReserveLevel};
use super::{
    BuddyAllocator, BuddyZone, FramePlacement, PageList, BUDDY_ORDER_COUNT, MAX_ORDER, MIN_ORDER,
};
use crate::mm::{page_reporting::ReportingBlock, MemoryManagementArch, PhysAddr};

pub(super) const REPORTED: usize = 1;
const MIN_REPORT_ORDER: usize = 21;
const SCAN_BUDGET: usize = 256;
const PAGE_SCAN_QUANTUM: usize = 16;

#[derive(Debug)]
pub(super) struct ReportingState {
    cursors: [[PhysAddr; BUDDY_ORDER_COUNT]; 2],
    next: usize,
    pub(super) spare: PhysAddr,
    pub(super) returning: bool,
    pending: Option<(PhysAddr, u8)>,
}

impl ReportingState {
    pub(super) const fn new() -> Self {
        Self {
            cursors: [[PhysAddr::new(0); BUDDY_ORDER_COUNT]; 2],
            next: 0,
            spare: PhysAddr::new(0),
            returning: false,
            pending: None,
        }
    }
}

impl<A: MemoryManagementArch> BuddyAllocator<A> {
    #[inline]
    pub(super) fn decode_entry(raw: PhysAddr) -> PhysAddr {
        PhysAddr::new(raw.data() & !REPORTED)
    }

    #[inline]
    pub(super) fn read_entry(metadata: PhysAddr, index: usize) -> PhysAddr {
        Self::decode_entry(unsafe { A::read(Self::entry_virt_addr(metadata, index)) })
    }

    pub(super) fn reporting_metadata_linked(
        &self,
        zone: BuddyZone,
        order: u8,
        metadata: PhysAddr,
    ) -> bool {
        let cursor = self.reporting.cursors[zone.index()][Self::order2index(order)];
        !cursor.is_null()
            && (cursor == metadata
                || Self::read_page::<PageList<A>>(metadata).report_next != metadata)
    }

    /// Only newly donated metadata is linked; recycled headers preserve the ring.
    pub(super) fn link_reporting_metadata(
        &mut self,
        zone: BuddyZone,
        order: u8,
        metadata: PhysAddr,
    ) -> PhysAddr {
        let cursor = &mut self.reporting.cursors[zone.index()][Self::order2index(order)];
        if cursor.is_null() {
            *cursor = metadata;
            metadata
        } else {
            let mut header: PageList<A> = Self::read_page(*cursor);
            let next = header.report_next;
            header.report_next = metadata;
            Self::write_page(*cursor, header);
            next
        }
    }

    pub(crate) fn take_reporting_block(&mut self) -> Option<ReportingBlock> {
        if self.reporting.pending.is_some() {
            return None;
        }
        // Prepay the only possible metadata growth during final insertion.
        // This page is never part of a device descriptor and cannot be borrowed
        // by ordinary allocator operations while the host owns the block.
        if self.reporting.spare.is_null() {
            self.reporting.spare = self
                .buddy_alloc_with_level(PageFrameCount::ONE, ReserveLevel::User)
                .ok()?
                .0;
        }
        let eligible_orders = MAX_ORDER - MIN_REPORT_ORDER;
        let mut budget = SCAN_BUDGET;
        while budget != 0 {
            budget -= 1;
            let turn = self.reporting.next;
            self.reporting.next = (turn + 1) % (eligible_orders * 2);
            let zone = if turn / eligible_orders == 0 {
                BuddyZone::Normal
            } else {
                BuddyZone::Dma32
            };
            let order = MIN_REPORT_ORDER + turn % eligible_orders;
            let slot = Self::order2index(order as u8);
            let metadata = self.reporting.cursors[zone.index()][slot];
            if metadata.is_null() {
                continue;
            }
            let mut header: PageList<A> = Self::read_page(metadata);
            self.reporting.cursors[zone.index()][slot] = header.report_next;
            let visits = header.entry_num.min(PAGE_SCAN_QUANTUM).min(budget);
            for _ in 0..visits {
                budget -= 1;
                let index = header.report_scan % header.entry_num;
                header.report_scan = (index + 1) % header.entry_num;
                let raw: PhysAddr = unsafe { A::read(Self::entry_virt_addr(metadata, index)) };
                if raw.data() & REPORTED != 0 {
                    continue;
                }
                let pages = 1 << (order - MIN_ORDER);
                // Reporting is optional global work, so preserve the same
                // low-memory inventory as unrestricted ordinary allocations.
                if (zone == BuddyZone::Dma32 && !self.dma32_fallback_allowed(pages))
                    || !self.admits(zone, pages, 0, ReserveLevel::User)
                {
                    continue;
                }
                Self::write_page(metadata, header);
                let address = self.remove_entry(
                    zone,
                    order as u8,
                    FramePlacement::Unmovable,
                    metadata,
                    index,
                );
                self.reporting.pending = Some((address, order as u8));
                return Some(ReportingBlock::new(address, order as u8));
            }
            Self::write_page(metadata, header);
        }
        None
    }

    pub(crate) fn finish_reporting_block(&mut self, block: ReportingBlock, reported: bool) {
        assert_eq!(self.reporting.pending, Some((block.address(), block.order)));
        self.reporting.pending = None;
        self.reporting.returning = true;
        // Same coalescing and quarantine rules as every ordinary frame free.
        unsafe { self.buddy_free_reported(block.address(), block.order, reported) };
        self.reporting.returning = false;
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
pub(crate) use selftest::report as selftest_report;
