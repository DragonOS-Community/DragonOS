use log::{debug, warn};

use super::{
    reserve::ReserveLevel,
    watermark::{ZonePressureSnapshot, ZoneWatermarks},
};
/// @Author: longjin@dragonos.org
/// @Author: kongweichao@dragonos.org
/// @Date: 2023-03-28 16:03:47
/// @FilePath: /DragonOS/kernel/src/mm/allocator/buddy.rs
/// @Description: 伙伴分配器
use crate::arch::MMArch;
use crate::mm::allocator::bump::BumpAllocator;
#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
use crate::mm::allocator::page_frame::{
    allocate_page_frames, deallocate_page_frames, PhysPageFrame,
};
use crate::mm::allocator::page_frame::{
    FrameAllocator, FramePlacement, PageFrameCount, PageFrameUsage,
};
use crate::mm::{MemoryManagementArch, PhysAddr, PhysMemoryArea, VirtAddr};

use core::cmp::min;
use core::fmt::Debug;
use core::fmt::Write;
use core::intrinsics::{likely, unlikely};

use core::{marker::PhantomData, mem};

// 一个全局变量MAX_ORDER，用来表示buddy算法的最大阶数 [MIN_ORDER, MAX_ORDER)左闭右开区间
const MAX_ORDER: usize = 31;
// 4KB
const MIN_ORDER: usize = 12;
pub(crate) const BUDDY_ORDER_COUNT: usize = MAX_ORDER - MIN_ORDER;
const DMA32_LIMIT: usize = u32::MAX as usize;
/// Keep 16 MiB of low memory available for devices which cannot address
/// Normal memory when an unrestricted allocation falls back to DMA32.
const DMA32_FALLBACK_RESERVE_PAGES: usize = (16 * 1024 * 1024) >> MIN_ORDER;
const DMA32_ALLOCATION_METADATA_HEADROOM: usize = placement::MAX_METADATA_HEADROOM;

pub(crate) mod compaction;
mod free_index;
mod placement;
use free_index::FreeIndex;
mod recovery;
use placement::{PlacementMap, BLOCK_ORDER, LOW_ORDERS};
mod reporting;
pub(crate) use recovery::{DmaAllocationFailure, DmaRecoveryZone};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuddyZone {
    Dma32,
    Normal,
}

impl BuddyZone {
    fn index(self) -> usize {
        match self {
            Self::Dma32 => 0,
            Self::Normal => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AllocationFailure {
    Capacity,
    Fragmentation,
    ZoneLimit,
    Reserve,
    AddressLimit,
    InvalidOrder,
}

const FAILURE_REASONS: [(&str, AllocationFailure); 6] = [
    ("capacity", AllocationFailure::Capacity),
    ("fragmentation", AllocationFailure::Fragmentation),
    ("zone_limit", AllocationFailure::ZoneLimit),
    ("reserve", AllocationFailure::Reserve),
    ("address_limit", AllocationFailure::AddressLimit),
    ("invalid_order", AllocationFailure::InvalidOrder),
];

#[derive(Clone, Copy, Debug, Default)]
struct ZoneDiagnostics {
    // Exact ownership counts, also used to prepay bounded range isolation.
    // These must remain unconditional and must not saturate like event totals.
    free_blocks: [usize; BUDDY_ORDER_COUNT],
    attempts: [u64; BUDDY_ORDER_COUNT],
    misses: [u64; BUDDY_ORDER_COUNT],
}

#[derive(Clone, Copy, Debug, Default)]
struct BuddyDiagnostics {
    placement_requests: [u64; 2],
    placement_fallback: [u64; 2],
    zones: [ZoneDiagnostics; 2],
    requests: [u64; BUDDY_ORDER_COUNT],
    failures: [u64; BUDDY_ORDER_COUNT],
    failure_reasons: [u64; FAILURE_REASONS.len()],
}

/// A bounded, allocation-free copy made under the allocator's existing lock.
/// Orders here count base pages (order 0 is one page), unlike the internal
/// byte orders. Attempts include fallback zones, but exclude internal splits
/// and metadata allocation. OOM retries are separate buddy requests.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BuddySnapshot {
    metadata_from_movable: u64,
    free_pages: [usize; 2],
    diagnostics: BuddyDiagnostics,
}

#[cfg(feature = "mm_selftest")]
impl BuddySnapshot {
    /// Pressure fixtures must budget against the address zone they consume.
    pub(crate) fn dma32_free_pages(&self) -> usize {
        self.free_pages[BuddyZone::Dma32.index()]
    }
}

/// 保存buddy算法中每一页存放的BuddyEntry的信息，占据每个页的起始位置
#[derive(Debug)]
#[repr(C)]
pub struct PageList<A> {
    // 页存放entry的数量
    entry_num: usize,
    order: u8,
    // 下一个页面的地址
    next_page: PhysAddr,
    // Permanent ring: metadata pages never return to ordinary free lists.
    report_next: PhysAddr,
    report_scan: usize,
    phantom: PhantomData<A>,
}

impl<A> Clone for PageList<A> {
    fn clone(&self) -> Self {
        Self {
            entry_num: self.entry_num,
            order: self.order,
            next_page: self.next_page,
            report_next: self.report_next,
            report_scan: self.report_scan,
            phantom: PhantomData,
        }
    }
}

impl<A> PageList<A> {
    fn with_order(mut self, order: u8) -> Self {
        self.order = order;
        self
    }

    #[allow(dead_code)]
    fn empty() -> Self {
        Self {
            entry_num: 0,
            order: 0,
            next_page: PhysAddr::new(0),
            report_next: PhysAddr::new(0),
            report_scan: 0,
            phantom: PhantomData,
        }
    }
    fn new(entry_num: usize, next_page: PhysAddr) -> Self {
        Self {
            entry_num,
            order: 0,
            next_page,
            report_next: PhysAddr::new(0),
            report_scan: 0,
            phantom: PhantomData,
        }
    }
}

/// @brief: 用来表示 buddy 算法中的一个 buddy 块，整体存放在area的头部
// 这种方式会出现对齐问题
// #[repr(packed)]
#[repr(C)]
#[derive(Debug)]
pub struct BuddyAllocator<A> {
    free_index: FreeIndex<A>,
    movable_free_area: [[PhysAddr; LOW_ORDERS]; 2],
    movable_blocks: [[usize; LOW_ORDERS]; 2],
    placement: PlacementMap,
    metadata_from_movable: u64,
    // DMA32 and normal memory have separate free lists.  This keeps a
    // 32-bit DMA allocation bounded by the number of buddy orders instead of
    // scanning every high-memory free block while interrupts are disabled.
    dma32_free_area: [PhysAddr; MAX_ORDER - MIN_ORDER],
    normal_free_area: [PhysAddr; MAX_ORDER - MIN_ORDER],
    dma32_empty_area: [PhysAddr; MAX_ORDER - MIN_ORDER],
    normal_empty_area: [PhysAddr; MAX_ORDER - MIN_ORDER],
    dma32_free_pages: usize,
    normal_free_pages: usize,
    watermarks: [ZoneWatermarks; 2],
    diagnostics: BuddyDiagnostics,
    isolation: Option<compaction::IsolationState>,
    next_isolation_id: u64,
    reporting: reporting::ReportingState,
    /// Whether this allocator has ever managed memory above 4 GiB.  Most
    /// current systems are DMA32-only; avoid probing every empty Normal order
    /// on their hot allocation path.
    has_normal_memory: bool,
    /// 总页数
    total: PageFrameCount,
    phantom: PhantomData<A>,
}

impl<A: MemoryManagementArch> BuddyAllocator<A> {
    /// Caller exclusively owns 39 metadata pages and the disjoint managed
    /// arena. Neither physical owner is transferred to the global allocator.
    #[cfg(all(
        feature = "mm_selftest",
        any(target_arch = "x86_64", target_arch = "riscv64")
    ))]
    pub(super) unsafe fn placement_fixture(
        metadata: PhysAddr,
        managed: PhysAddr,
        pages: usize,
    ) -> Option<Self> {
        let bytes = pages.checked_mul(A::PAGE_SIZE)?;
        let end = managed.data().checked_add(bytes)?;
        let metadata_end = metadata.data().checked_add(39 * A::PAGE_SIZE)?;
        if pages == 0
            || !managed.check_aligned(A::PAGE_SIZE)
            || !metadata.check_aligned(A::PAGE_SIZE)
            || metadata.data() < end && managed.data() < metadata_end
        {
            return None;
        }
        let mut dma32_empty_area = [PhysAddr::new(0); BUDDY_ORDER_COUNT];
        let mut normal_empty_area = [PhysAddr::new(0); BUDDY_ORDER_COUNT];
        for (index, slot) in dma32_empty_area
            .iter_mut()
            .chain(normal_empty_area.iter_mut())
            .enumerate()
        {
            *slot = metadata + index * A::PAGE_SIZE;
            Self::write_page(*slot, PageList::new(0, PhysAddr::new(0)));
        }
        let areas = [PhysMemoryArea::new(
            managed,
            bytes,
            crate::mm::memblock::MemoryAreaAttr::empty(),
        )];
        let (count, blocks, _) = PlacementMap::requirements(&areas)?;
        let map = PlacementMap::initialize::<A>(
            metadata + 38 * A::PAGE_SIZE,
            A::PAGE_SIZE,
            count,
            blocks,
            &areas,
        )?;
        Some(Self {
            dma32_free_area: [PhysAddr::new(0); BUDDY_ORDER_COUNT],
            normal_free_area: [PhysAddr::new(0); BUDDY_ORDER_COUNT],
            movable_free_area: [[PhysAddr::new(0); LOW_ORDERS]; 2],
            movable_blocks: [[0; LOW_ORDERS]; 2],
            placement: map,
            free_index: FreeIndex::fixture(&areas)?,
            metadata_from_movable: 0,
            dma32_empty_area,
            normal_empty_area,
            dma32_free_pages: 0,
            normal_free_pages: 0,
            watermarks: [ZoneWatermarks::default(); 2],
            diagnostics: BuddyDiagnostics::default(),
            isolation: None,
            next_isolation_id: 1,
            reporting: reporting::ReportingState::new(),
            has_normal_memory: false,
            total: PageFrameCount::new(pages),
            phantom: PhantomData,
        })
    }

    /// Resolve a free head without reading its page payload. A locator always
    /// points into a live PageList, whose header carries the byte order.
    fn free_entry(&self, base: PhysAddr) -> Option<(u8, PhysAddr, usize)> {
        let entry = self.free_index.lookup(base)?;
        let metadata = PhysAddr::new(entry.data() & !(A::PAGE_SIZE - 1));
        let list: PageList<A> = Self::read_page(metadata);
        assert!((MIN_ORDER..MAX_ORDER).contains(&(list.order as usize)));
        let offset = entry
            .data()
            .checked_sub(metadata.data() + mem::size_of::<PageList<A>>())
            .expect("free index points into a metadata header");
        assert_eq!(offset % mem::size_of::<PhysAddr>(), 0);
        let index = offset / mem::size_of::<PhysAddr>();
        assert!(index < list.entry_num);
        let stored: PhysAddr = unsafe { A::read(A::phys_2_virt(entry).unwrap()) };
        assert_eq!(
            Self::decode_entry(stored),
            base,
            "free index points at a different block"
        );
        Some((list.order, metadata, index))
    }

    fn index_next_head(
        &self,
        order: u8,
        start: PhysAddr,
        end: PhysAddr,
        remaining: &mut usize,
    ) -> Result<Option<PhysAddr>, compaction::RangeError> {
        self.free_index.next_head(order, start, end, remaining)
    }

    fn index_isolation_bound(&self, start: PhysAddr, pages: usize) -> Option<usize> {
        self.free_index
            .isolation_bound(start, pages)?
            .checked_add(self.placement.lookup_work_bound())
    }

    fn index_lookup_bound(&self) -> usize {
        self.free_index.lookup_bound()
    }

    fn compaction_point_bound(&self, points: usize) -> Option<usize> {
        // A point can classify an insertion and retag a metadata/pageblock
        // source. Both use the placement map's independent span lookup.
        self.free_index
            .point_mutation_bound(points)?
            .checked_add(points.checked_mul(2 * self.placement.lookup_work_bound())?)
    }

    #[cfg(feature = "mm_selftest")]
    fn fixture_index(backing: PhysAddr, pages: usize) -> FreeIndex<A> {
        use crate::mm::memblock::MemoryAreaAttr;
        // Explicit physical identities used by these private DMA/watermark
        // models. None of their payloads is accessed by the index.
        let mut areas = alloc::vec![
            PhysMemoryArea::new(backing, pages * A::PAGE_SIZE, MemoryAreaAttr::empty()),
            PhysMemoryArea::new(
                PhysAddr::new(0x0200_0000),
                64 * 1024 * 1024,
                MemoryAreaAttr::empty()
            ),
            PhysMemoryArea::new(
                PhysAddr::new(0x1000_0000),
                16 * 1024 * 1024,
                MemoryAreaAttr::empty()
            ),
            PhysMemoryArea::new(
                PhysAddr::new(0x3000_0000),
                16 * 1024 * 1024,
                MemoryAreaAttr::empty()
            ),
            PhysMemoryArea::new(
                PhysAddr::new(2usize << 30),
                16 * 1024 * 1024,
                MemoryAreaAttr::empty()
            ),
            PhysMemoryArea::new(
                PhysAddr::new((1usize << 32) - 16 * 1024 * 1024),
                32 * 1024 * 1024,
                MemoryAreaAttr::empty()
            ),
        ];
        areas.sort_unstable_by_key(|area| area.base);
        let mut used = 0;
        for index in 0..areas.len() {
            let area = areas[index];
            if used != 0 {
                let previous = &mut areas[used - 1];
                let end = previous.base.data() + previous.size;
                if area.base.data() <= end {
                    previous.size = end.max(area.base.data() + area.size) - previous.base.data();
                    continue;
                }
            }
            areas[used] = area;
            used += 1;
        }
        FreeIndex::fixture(&areas[..used]).expect("private buddy index backing")
    }

    fn zone_free(&self, zone: BuddyZone) -> usize {
        match zone {
            BuddyZone::Dma32 => self.dma32_free_pages,
            BuddyZone::Normal => self.normal_free_pages,
        }
    }

    fn floor(&self, zone: BuddyZone, level: ReserveLevel) -> usize {
        let marks = self.watermarks[zone.index()];
        match level {
            ReserveLevel::User => marks.low,
            ReserveLevel::Kernel => marks.min,
            ReserveLevel::OomExit => marks.min / 2,
            ReserveLevel::Reclaim => 0,
        }
    }

    fn admits(&self, zone: BuddyZone, count: usize, metadata: usize, level: ReserveLevel) -> bool {
        self.zone_free(zone)
            .checked_sub(count)
            .is_some_and(|remaining| remaining >= self.floor(zone, level).saturating_add(metadata))
    }

    /// Only destinations of this split can start metadata growth. The source
    /// order is removed, never inserted, and its occupied list is irrelevant.
    fn split_metadata_headroom(&self, zone: BuddyZone, requested: u8, source: u8) -> usize {
        self.split_headroom_for(zone, requested, source, FramePlacement::Unmovable)
    }

    fn split_headroom_for(
        &self,
        zone: BuddyZone,
        requested: u8,
        source: u8,
        split_class: FramePlacement,
    ) -> usize {
        for order in (requested..source).rev() {
            let class = if (order as usize) < BLOCK_ORDER {
                split_class
            } else {
                FramePlacement::Unmovable
            };
            let head = self.class_area(zone, order, class);
            let needs = head.is_null()
                || Self::read_page::<PageList<A>>(head).entry_num == Self::BUDDY_ENTRIES;
            if needs && self.empty_area(zone, order).is_null() {
                // At most one new metadata page per class/order. Growth only
                // inserts into lower orders, <=38 slots < a metadata page.
                assert!(Self::BUDDY_ENTRIES > 2 * BUDDY_ORDER_COUNT);
                let orders = Self::order2index(order) + 1;
                return orders
                    + if order as usize >= BLOCK_ORDER && split_class == FramePlacement::Movable {
                        LOW_ORDERS
                    } else {
                        0
                    };
            }
        }
        0
    }

    fn class_blocks(&self, zone: BuddyZone, order: usize, class: FramePlacement) -> usize {
        let index = order - MIN_ORDER;
        let movable = if order < BLOCK_ORDER {
            self.movable_blocks[zone.index()][index]
        } else {
            0
        };
        if class == FramePlacement::Movable {
            movable
        } else {
            self.diagnostics.zones[zone.index()].free_blocks[index] - movable
        }
    }

    fn record_placement_fallback(&mut self, base: PhysAddr, class: FramePlacement) {
        if self.placement.get::<A>(base) != class {
            let counter = &mut self.diagnostics.placement_fallback[class as usize];
            *counter = counter.saturating_add(1);
        }
    }

    #[cfg(feature = "mm_selftest")]
    pub(crate) fn placement_of(&self, base: PhysAddr) -> FramePlacement {
        self.placement.get::<A>(base)
    }

    #[cfg(feature = "mm_selftest")]
    pub(crate) fn placement_statistics(&self) -> ([u64; 2], [u64; 2]) {
        (
            self.diagnostics.placement_requests,
            self.diagnostics.placement_fallback,
        )
    }

    fn classify_allocation(
        &mut self,
        base: PhysAddr,
        source: u8,
        requested: u8,
        class: FramePlacement,
    ) {
        if source as usize >= BLOCK_ORDER {
            let label_order = (requested as usize).max(BLOCK_ORDER);
            let block = PhysAddr::new(base.data() & !((1usize << label_order) - 1));
            self.placement.set::<A>(block, label_order, class);
        }
    }

    pub(crate) fn pressure_snapshot(&self) -> ZonePressureSnapshot {
        ZonePressureSnapshot {
            dma32_deficit: self.watermarks[0]
                .high
                .saturating_sub(self.dma32_free_pages),
            normal_deficit: self.watermarks[1]
                .high
                .saturating_sub(self.normal_free_pages),
        }
    }

    fn request_pressure_if_needed(&self, failed: bool) {
        let low = self.watermarks.iter().enumerate().any(|(index, marks)| {
            marks.low != 0 && [self.dma32_free_pages, self.normal_free_pages][index] <= marks.low
        });
        if low || failed && self.pressure_snapshot().total_deficit() != 0 {
            crate::mm::page::reclaim_wait::request_pressure();
        }
    }

    /// One handoff invariant for ordinary, bounded and typed DMA attempts.
    fn finish_admitted(
        &self,
        physical: PhysAddr,
        count: PageFrameCount,
        level: ReserveLevel,
    ) -> (PhysAddr, PageFrameCount) {
        let zone = Self::zone_for(physical, MIN_ORDER);
        assert!(self.zone_free(zone) >= self.floor(zone, level));
        (physical, count)
    }

    pub(crate) fn allocate_admitted(
        &mut self,
        request: super::page_frame::FrameAllocationRequest,
        level: ReserveLevel,
    ) -> Option<(PhysAddr, PageFrameCount)> {
        let request = request.normalized()?;
        let result = self
            .buddy_alloc_below_with_placement(
                request.count,
                request.max_phys_addr,
                level,
                request.placement,
            )
            .ok();
        self.request_pressure_if_needed(result.is_none());
        result
    }

    pub(crate) fn held_handoff_allowed(&self, physical: PhysAddr, level: ReserveLevel) -> bool {
        let zone = if physical.data() <= DMA32_LIMIT {
            BuddyZone::Dma32
        } else {
            BuddyZone::Normal
        };
        self.admits(zone, 0, 0, level)
    }

    const BUDDY_ENTRIES: usize =
        // 定义一个变量记录buddy表的大小
        (A::PAGE_SIZE - mem::size_of::<PageList<A>>()) / mem::size_of::<PhysAddr>();

    #[inline(never)]
    pub unsafe fn new(mut bump_allocator: BumpAllocator<A>) -> Option<Self> {
        let initial_free_pages = bump_allocator.usage().free();
        let total_memory = bump_allocator.usage().total();
        debug!("Free pages before init buddy: {:?}", initial_free_pages);
        // debug!("Buddy entries: {}", Self::BUDDY_ENTRIES);

        let dma32_free_area = [PhysAddr::new(0); MAX_ORDER - MIN_ORDER];
        let normal_free_area = [PhysAddr::new(0); MAX_ORDER - MIN_ORDER];
        let mut dma32_empty_area = [PhysAddr::new(0); MAX_ORDER - MIN_ORDER];
        let mut normal_empty_area = [PhysAddr::new(0); MAX_ORDER - MIN_ORDER];

        // Buddy初始占用的空间从bump分配
        for f in dma32_empty_area
            .iter_mut()
            .chain(normal_empty_area.iter_mut())
        {
            let curr_page = bump_allocator.allocate_one();
            // 保存每个阶的空闲链表的头部地址
            *f = curr_page.unwrap();
            // 清空当前页
            core::ptr::write_bytes(MMArch::phys_2_virt(*f)?.data() as *mut u8, 0, A::PAGE_SIZE);

            let page_list: PageList<A> = PageList::new(0, PhysAddr::new(0));
            Self::write_page(*f, page_list);
        }

        let mut total_pages_to_buddy = PageFrameCount::new(0);
        static mut RES_AREAS: [PhysMemoryArea;
            crate::mm::memblock::INITIAL_MEMORY_REGIONS_NUM * 2] =
            [PhysMemoryArea::DEFAULT; crate::mm::memblock::INITIAL_MEMORY_REGIONS_NUM * 2];
        let (count, offset) = bump_allocator.remain_areas(&mut RES_AREAS)?;
        let count = PlacementMap::normalize::<A>(&mut RES_AREAS[..count], offset)?;
        let (max_spans, max_blocks, bytes) = PlacementMap::requirements(&RES_AREAS[..count])?;
        let metadata_pages = bytes.checked_add(A::PAGE_SIZE - 1)? / A::PAGE_SIZE;
        let metadata_pages = metadata_pages.max(1);
        let (storage, actual) = bump_allocator.allocate(PageFrameCount::new(metadata_pages))?;
        if actual.data() != metadata_pages {
            return None;
        }
        let free_index =
            FreeIndex::<A>::initialize(&RES_AREAS[..count], || bump_allocator.allocate_one())?;
        let (count, offset) = bump_allocator.remain_areas(&mut RES_AREAS)?;
        let count = PlacementMap::normalize::<A>(&mut RES_AREAS[..count], offset)?;
        let remain_areas = &RES_AREAS[..count];
        let placement = PlacementMap::initialize::<A>(
            storage,
            metadata_pages.checked_mul(A::PAGE_SIZE)?,
            max_spans,
            max_blocks,
            remain_areas,
        )?;
        let mut allocator = Self {
            dma32_free_area,
            normal_free_area,
            movable_free_area: [[PhysAddr::new(0); LOW_ORDERS]; 2],
            movable_blocks: [[0; LOW_ORDERS]; 2],
            placement,
            free_index,
            metadata_from_movable: 0,
            dma32_empty_area,
            normal_empty_area,
            dma32_free_pages: 0,
            normal_free_pages: 0,
            watermarks: [ZoneWatermarks::default(); 2],
            diagnostics: BuddyDiagnostics::default(),
            isolation: None,
            next_isolation_id: 1,
            reporting: reporting::ReportingState::new(),
            has_normal_memory: false,
            total: PageFrameCount::new(0),
            phantom: PhantomData,
        };

        for area in remain_areas {
            let mut paddr = area.base.data();
            let mut remain_pages =
                PageFrameCount::from_bytes(area.area_end_aligned().data() - paddr).unwrap();

            if remain_pages.data() == 0 {
                continue;
            }
            // debug!("area: {area:?}, paddr: {paddr:#x}, remain_pages: {remain_pages:?}");

            total_pages_to_buddy += remain_pages;

            // 先从低阶开始，尽可能地填满空闲链表
            for i in MIN_ORDER..MAX_ORDER {
                // debug!("i {i}, remain pages={}", remain_pages.data());
                if remain_pages.data() < (1 << (i - MIN_ORDER)) {
                    break;
                }

                assert!(paddr & ((1 << i) - 1) == 0);

                if likely(i != MAX_ORDER - 1) {
                    // 要填写entry
                    if paddr & (1 << i) != 0 {
                        allocator.buddy_free(PhysAddr::new(paddr), i as u8);

                        paddr += 1 << i;
                        remain_pages -= 1 << (i - MIN_ORDER);
                    };
                } else {
                    // 往最大的阶数的链表中添加entry（注意要考虑到最大阶数的链表可能有多页）
                    // 断言剩余页面数量是MAX_ORDER-1阶的整数倍

                    let mut entries = (remain_pages.data() * A::PAGE_SIZE) >> i;
                    while entries > 0 {
                        allocator.buddy_free(PhysAddr::new(paddr), i as u8);
                        paddr += 1 << i;
                        remain_pages -= 1 << (i - MIN_ORDER);

                        entries -= 1;
                    }
                }
            }
            // 然后从高往低，把剩余的页面加入链表
            let mut remain_bytes = remain_pages.data() * A::PAGE_SIZE;

            assert!(remain_bytes < (1 << MAX_ORDER) - 1);

            for i in (MIN_ORDER..MAX_ORDER).rev() {
                if remain_bytes >= (1 << i) {
                    assert!(paddr & ((1 << i) - 1) == 0);
                    allocator.buddy_free(PhysAddr::new(paddr), i as u8);

                    paddr += 1 << i;
                    remain_bytes -= 1 << i;
                }
            }

            assert!(remain_bytes == 0);
        }

        debug!("Total pages to buddy: {:?}", total_pages_to_buddy);
        allocator.total = total_memory;
        allocator.watermarks = super::watermark::calculate(
            [allocator.dma32_free_pages, allocator.normal_free_pages],
            A::PAGE_SIZE,
        );

        Some(allocator)
    }
    /// 获取第j个entry的虚拟地址，
    /// j从0开始计数
    pub fn entry_virt_addr(base_addr: PhysAddr, j: usize) -> VirtAddr {
        let entry_virt_addr = unsafe { A::phys_2_virt(Self::entry_addr(base_addr, j)) };
        return entry_virt_addr.unwrap();
    }
    pub fn entry_addr(base_addr: PhysAddr, j: usize) -> PhysAddr {
        let entry_addr = base_addr + mem::size_of::<PageList<A>>() + j * mem::size_of::<PhysAddr>();
        return entry_addr;
    }
    pub fn read_page<T>(addr: PhysAddr) -> T {
        let page_list = unsafe { A::read(A::phys_2_virt(addr).unwrap()) };
        return page_list;
    }

    pub fn write_page(curr_page: PhysAddr, mut page_list: PageList<A>) {
        if page_list.report_next.is_null() {
            page_list.report_next = curr_page;
        }
        // 把物理地址转换为虚拟地址
        let virt_addr = unsafe { A::phys_2_virt(curr_page) };
        let virt_addr = virt_addr.unwrap();
        unsafe { A::write(virt_addr, page_list) };
    }

    /// 从order转换为free_area的下标
    ///
    /// # 参数
    ///
    /// - `order` - order
    ///
    /// # 返回值
    ///
    /// free_area的下标
    #[inline]
    fn order2index(order: u8) -> usize {
        order as usize - MIN_ORDER
    }

    #[inline]
    fn zone_for(base: PhysAddr, order: usize) -> BuddyZone {
        let end = base
            .data()
            .checked_add((1usize << order) - 1)
            .expect("buddy block address overflow");
        assert!(
            base.data() > DMA32_LIMIT || end <= DMA32_LIMIT,
            "buddy block crosses the DMA32 boundary"
        );
        if end <= DMA32_LIMIT {
            BuddyZone::Dma32
        } else {
            BuddyZone::Normal
        }
    }

    #[inline]
    fn free_area(&self, zone: BuddyZone, order: u8) -> PhysAddr {
        self.class_area(zone, order, FramePlacement::Unmovable)
    }

    #[cfg(feature = "mm_selftest")]
    fn set_free_area(&mut self, zone: BuddyZone, order: u8, value: PhysAddr) {
        self.set_class_area(zone, order, FramePlacement::Unmovable, value);
    }

    fn class_area(&self, zone: BuddyZone, order: u8, class: FramePlacement) -> PhysAddr {
        if class == FramePlacement::Movable {
            assert!((order as usize) < BLOCK_ORDER);
            return self.movable_free_area[zone.index()][Self::order2index(order)];
        }
        let index = Self::order2index(order);
        match zone {
            BuddyZone::Dma32 => self.dma32_free_area[index],
            BuddyZone::Normal => self.normal_free_area[index],
        }
    }

    #[inline]
    fn set_class_area(
        &mut self,
        zone: BuddyZone,
        order: u8,
        class: FramePlacement,
        value: PhysAddr,
    ) {
        if class == FramePlacement::Movable {
            assert!((order as usize) < BLOCK_ORDER);
            self.movable_free_area[zone.index()][Self::order2index(order)] = value;
            return;
        }
        let index = Self::order2index(order);
        match zone {
            BuddyZone::Dma32 => self.dma32_free_area[index] = value,
            BuddyZone::Normal => self.normal_free_area[index] = value,
        }
    }

    #[inline]
    fn empty_area(&self, zone: BuddyZone, order: u8) -> PhysAddr {
        let index = Self::order2index(order);
        match zone {
            BuddyZone::Dma32 => self.dma32_empty_area[index],
            BuddyZone::Normal => self.normal_empty_area[index],
        }
    }

    #[inline]
    fn set_empty_area(&mut self, zone: BuddyZone, order: u8, value: PhysAddr) {
        let index = Self::order2index(order);
        match zone {
            BuddyZone::Dma32 => self.dma32_empty_area[index] = value,
            BuddyZone::Normal => self.normal_empty_area[index] = value,
        }
    }

    #[inline]
    fn adjust_free_pages(&mut self, zone: BuddyZone, pages: usize, add: bool) {
        let free_pages = match zone {
            BuddyZone::Dma32 => &mut self.dma32_free_pages,
            BuddyZone::Normal => &mut self.normal_free_pages,
        };
        if add {
            *free_pages = free_pages
                .checked_add(pages)
                .expect("buddy free count overflow");
        } else {
            *free_pages = free_pages
                .checked_sub(pages)
                .expect("buddy free count underflow");
        }
    }

    pub(crate) fn diagnostic_snapshot(&self) -> BuddySnapshot {
        BuddySnapshot {
            metadata_from_movable: self.metadata_from_movable,
            free_pages: [self.dma32_free_pages, self.normal_free_pages],
            diagnostics: self.diagnostics,
        }
    }

    fn record_request(&mut self, count: PageFrameCount, placement: FramePlacement) -> Option<u8> {
        assert!(count.data().is_power_of_two());
        let order = log2(count.data());
        if order >= BUDDY_ORDER_COUNT {
            self.record_failure(None, AllocationFailure::InvalidOrder);
            return None;
        }
        self.diagnostics.placement_requests[placement as usize] =
            self.diagnostics.placement_requests[placement as usize].saturating_add(1);
        self.diagnostics.requests[order] = self.diagnostics.requests[order].saturating_add(1);
        Some((order + MIN_ORDER) as u8)
    }

    fn record_failure(&mut self, order: Option<u8>, reason: AllocationFailure) {
        if let Some(order) = order {
            let failures = &mut self.diagnostics.failures[Self::order2index(order)];
            *failures = failures.saturating_add(1);
        }
        let failures = &mut self.diagnostics.failure_reasons[reason as usize];
        *failures = failures.saturating_add(1);
    }

    fn try_zone(
        &mut self,
        zone: BuddyZone,
        order: u8,
        level: ReserveLevel,
        placement: FramePlacement,
    ) -> Result<PhysAddr, AllocationFailure> {
        let needed = 1usize << (order as usize - MIN_ORDER);
        let index = Self::order2index(order);
        self.diagnostics.zones[zone.index()].attempts[index] =
            self.diagnostics.zones[zone.index()].attempts[index].saturating_add(1);
        let mut denied = false;
        for (source, class) in placement::allocation_chains(order, placement) {
            let head = self.class_area(zone, source, class);
            if head.is_null() {
                continue;
            }
            let split_class = if source as usize >= BLOCK_ORDER {
                placement
            } else {
                class
            };
            let metadata = self.split_headroom_for(zone, order, source, split_class);
            if !self.admits(zone, needed, metadata, level) {
                denied = true;
                continue;
            }
            let list: PageList<A> = Self::read_page(head);
            let base = self.remove_entry(zone, source, class, head, list.entry_num - 1);
            self.classify_allocation(base, source, order, placement);
            self.record_placement_fallback(base, placement);
            let mut current = source;
            while current > order {
                current -= 1;
                unsafe {
                    self.insert_without_merge(base + (1usize << current), current);
                }
            }
            return Ok(base);
        }
        self.diagnostics.zones[zone.index()].misses[index] =
            self.diagnostics.zones[zone.index()].misses[index].saturating_add(1);
        Err(if denied {
            AllocationFailure::Reserve
        } else {
            AllocationFailure::Fragmentation
        })
    }

    fn lacks_capacity(&self, count: PageFrameCount) -> bool {
        self.dma32_free_pages + self.normal_free_pages < count.data()
    }

    fn recycle_empty_metadata(
        &mut self,
        zone: BuddyZone,
        order: u8,
        class: FramePlacement,
        page_addr: PhysAddr,
        next_nonempty: PhysAddr,
    ) {
        debug_assert_eq!(self.class_area(zone, order, class), page_addr);
        self.set_class_area(zone, order, class, next_nonempty);
        let empty_head = self.empty_area(zone, order);
        let mut header: PageList<A> = Self::read_page(page_addr);
        header.entry_num = 0;
        header.order = order;
        header.next_page = empty_head;
        header.report_scan = 0;
        Self::write_page(page_addr, header);
        self.set_empty_area(zone, order, page_addr);
    }

    /// Remove one entry from the non-empty chain head.  If the head becomes
    /// empty it is unlinked immediately and moved to the reusable metadata
    /// chain, so allocation never walks historical empty pages.
    fn remove_entry(
        &mut self,
        zone: BuddyZone,
        order: u8,
        class: FramePlacement,
        target_page_addr: PhysAddr,
        target_index: usize,
    ) -> PhysAddr {
        let mut target_page: PageList<A> = Self::read_page(target_page_addr);
        assert!(target_index < target_page.entry_num);
        let removed: PhysAddr =
            unsafe { A::read(Self::entry_virt_addr(target_page_addr, target_index)) };
        let first_addr = self.class_area(zone, order, class);
        assert!(!first_addr.is_null());
        let mut first: PageList<A> = Self::read_page(first_addr);
        assert!(first.entry_num > 0);
        let last_index = first.entry_num - 1;
        let last: PhysAddr = unsafe { A::read(Self::entry_virt_addr(first_addr, last_index)) };
        self.free_index.remove(
            Self::decode_entry(removed),
            Self::entry_addr(target_page_addr, target_index),
            order,
        );
        if target_page_addr != first_addr || target_index != last_index {
            self.free_index.relocate(
                Self::decode_entry(last),
                Self::entry_addr(target_page_addr, target_index),
            );
        }

        if target_page_addr == first_addr {
            if target_index != last_index {
                unsafe {
                    A::write(Self::entry_virt_addr(first_addr, target_index), last);
                }
            }
            first.entry_num -= 1;
            first.report_scan = 0;
            unsafe {
                A::write(
                    Self::entry_virt_addr(first_addr, first.entry_num),
                    PhysAddr::new(0),
                );
            }
            let next_nonempty = first.next_page;
            Self::write_page(first_addr, first);
            if last_index == 0 {
                self.recycle_empty_metadata(zone, order, class, first_addr, next_nonempty);
            }
        } else {
            unsafe {
                A::write(Self::entry_virt_addr(target_page_addr, target_index), last);
                A::write(
                    Self::entry_virt_addr(first_addr, last_index),
                    PhysAddr::new(0),
                );
            }
            first.entry_num -= 1;
            first.report_scan = 0;
            let next_nonempty = first.next_page;
            Self::write_page(first_addr, first);
            if last_index == 0 {
                self.recycle_empty_metadata(zone, order, class, first_addr, next_nonempty);
            }
            // The target page remains non-empty because the removed slot was
            // replaced from the first non-empty page.
            target_page = Self::read_page(target_page_addr);
            target_page.report_scan = 0;
            Self::write_page(target_page_addr, target_page.clone());
            debug_assert!(target_page.entry_num > 0);
        }

        self.adjust_free_pages(zone, 1usize << (order as usize - MIN_ORDER), false);
        self.diagnostics.zones[zone.index()].free_blocks[Self::order2index(order)] -= 1;
        if class == FramePlacement::Movable {
            self.movable_blocks[zone.index()][Self::order2index(order)] -= 1;
        }
        Self::decode_entry(removed)
    }

    /// Insert a block whose buddy is known to be allocated.  This is used for
    /// halves produced by an allocation split, avoiding a pointless linear
    /// buddy search on the IRQ-off allocation path.
    unsafe fn prepare_metadata_page(&mut self, base: PhysAddr, order: u8, class: FramePlacement) {
        if order as usize >= BLOCK_ORDER {
            self.placement
                .set::<A>(base, BLOCK_ORDER, FramePlacement::Unmovable);
        } else if class == FramePlacement::Movable {
            self.metadata_from_movable = self.metadata_from_movable.saturating_add(1);
        }
        core::ptr::write_bytes(
            A::phys_2_virt(base).unwrap().as_ptr::<u8>(),
            0,
            A::PAGE_SIZE,
        );
    }

    unsafe fn insert_without_merge(&mut self, base: PhysAddr, order: u8) {
        let zone = Self::zone_for(base, order as usize);
        if zone == BuddyZone::Normal {
            self.has_normal_memory = true;
        }
        let class = if (order as usize) < BLOCK_ORDER {
            self.placement.get::<A>(base)
        } else {
            FramePlacement::Unmovable
        };
        let mut first_addr = self.class_area(zone, order, class);
        let needs_metadata = first_addr.is_null()
            || Self::read_page::<PageList<A>>(first_addr).entry_num == Self::BUDDY_ENTRIES;
        if needs_metadata {
            let empty = self.empty_area(zone, order);
            let reused = !empty.is_null();
            let metadata = if reused {
                let recycled: PageList<A> = Self::read_page(empty);
                self.set_empty_area(zone, order, recycled.next_page);
                empty
            } else if self.reporting.returning && !self.reporting.spare.is_null() {
                let spare = self.reporting.spare;
                self.reporting.spare = PhysAddr::new(0);
                // The prepaid owner is one page, not a target-order extent.
                // Preserve its pageblock label; retagging a live pageblock
                // would invalidate its existing low-order class chains.
                let placement = self.placement.get::<A>(spare);
                self.prepare_metadata_page(spare, MIN_ORDER as u8, placement);
                spare
            } else if !first_addr.is_null() && order as usize > MIN_ORDER {
                // An already full same-class head proves this exact source
                // exists. Preserve the incoming outside sibling as a whole;
                // do not call the general allocator or search higher orders.
                let head: PageList<A> = Self::read_page(first_addr);
                let source = self.remove_entry(zone, order, class, first_addr, head.entry_num - 1);
                self.prepare_metadata_page(source, order, class);
                for lower in (MIN_ORDER..order as usize).rev() {
                    self.insert_without_merge(source + (1usize << lower), lower as u8);
                }
                first_addr = self.class_area(zone, order, class);
                source
            } else {
                // A new class may have no source and no shared spare. The
                // incoming extent terminates self-hosting without recursion
                // to the same or any higher order.
                self.prepare_metadata_page(base, order, class);
                let report_next = self.link_reporting_metadata(zone, order, base);
                let mut header = PageList::new(0, PhysAddr::new(0)).with_order(order);
                header.report_next = report_next;
                Self::write_page(base, header);
                self.set_empty_area(zone, order, base);
                for lower in (MIN_ORDER..order as usize).rev() {
                    self.insert_without_merge(base + (1usize << lower), lower as u8);
                }
                return;
            };
            let report_next = if reused && self.reporting_metadata_linked(zone, order, metadata) {
                Self::read_page::<PageList<A>>(metadata).report_next
            } else {
                self.link_reporting_metadata(zone, order, metadata)
            };
            let mut header = PageList::new(0, first_addr).with_order(order);
            header.report_next = report_next;
            Self::write_page(metadata, header);
            self.set_class_area(zone, order, class, metadata);
            first_addr = metadata;
        }
        let mut head: PageList<A> = Self::read_page(first_addr);
        assert!(head.entry_num < Self::BUDDY_ENTRIES);
        A::write(Self::entry_virt_addr(first_addr, head.entry_num), base);
        head.entry_num += 1;
        let slot = Self::entry_addr(first_addr, head.entry_num - 1);
        head.report_scan = 0;
        Self::write_page(first_addr, head);
        self.free_index.insert(base, slot, order);
        self.adjust_free_pages(zone, 1usize << (order as usize - MIN_ORDER), true);
        self.diagnostics.zones[zone.index()].free_blocks[Self::order2index(order)] += 1;
        if class == FramePlacement::Movable {
            self.movable_blocks[zone.index()][Self::order2index(order)] += 1;
        }
    }

    #[cfg(feature = "mm_selftest")]
    fn pop_front(&mut self, zone: BuddyZone, order: u8) -> Option<PhysAddr> {
        self.try_zone(
            zone,
            order,
            ReserveLevel::Reclaim,
            FramePlacement::Unmovable,
        )
        .ok()
    }

    /// 从伙伴系统中分配count个页面
    ///
    /// ## 参数
    ///
    /// - `count`：需要分配的页面数
    ///
    /// ## 返回值
    ///
    /// 返回分配的页面的物理地址和页面数
    fn buddy_alloc(&mut self, count: PageFrameCount) -> Option<(PhysAddr, PageFrameCount)> {
        self.buddy_alloc_result(count).ok()
    }

    /// Whether an unrestricted allocation may consume low memory. A run
    /// already held outside the free lists passes zero at final handoff.
    pub(crate) fn dma32_fallback_allowed(&self, needed: usize) -> bool {
        !self.has_normal_memory
            || self.dma32_free_pages
                >= needed
                    .saturating_add(DMA32_FALLBACK_RESERVE_PAGES)
                    .saturating_add(DMA32_ALLOCATION_METADATA_HEADROOM)
    }

    fn buddy_alloc_result(
        &mut self,
        count: PageFrameCount,
    ) -> Result<(PhysAddr, PageFrameCount), AllocationFailure> {
        self.buddy_alloc_with_level(count, ReserveLevel::Kernel)
    }

    fn buddy_alloc_with_level(
        &mut self,
        count: PageFrameCount,
        level: ReserveLevel,
    ) -> Result<(PhysAddr, PageFrameCount), AllocationFailure> {
        self.buddy_alloc_with_placement(count, level, FramePlacement::Unmovable)
    }

    fn buddy_alloc_with_placement(
        &mut self,
        count: PageFrameCount,
        level: ReserveLevel,
        placement: FramePlacement,
    ) -> Result<(PhysAddr, PageFrameCount), AllocationFailure> {
        let order = self
            .record_request(count, placement)
            .ok_or(AllocationFailure::InvalidOrder)?;

        // debug!("buddy_alloc: order = {}", order);
        // 获取该阶数的一个空闲页面
        // Preserve scarce DMA32 memory while normal memory is available.
        let normal = if self.has_normal_memory {
            Some(self.try_zone(BuddyZone::Normal, order, level, placement))
        } else {
            None
        };
        let mut admission_denied = matches!(normal, Some(Err(AllocationFailure::Reserve)));
        let dma32_fallback_allowed = self.dma32_fallback_allowed(count.data());
        let free_addr = normal.and_then(Result::ok).or_else(|| {
            if !dma32_fallback_allowed {
                return None;
            }
            match self.try_zone(BuddyZone::Dma32, order, level, placement) {
                Ok(base) => Some(base),
                Err(reason) => {
                    admission_denied |= reason == AllocationFailure::Reserve;
                    None
                }
            }
        });
        if free_addr.is_none() {
            let reason = if self.lacks_capacity(count) {
                AllocationFailure::Capacity
            } else if admission_denied
                || (!dma32_fallback_allowed && self.dma32_free_pages >= count.data())
                || (self.dma32_free_pages >= count.data() || self.normal_free_pages >= count.data())
                    && !self.admits(BuddyZone::Dma32, count.data(), 0, level)
                    && (!self.has_normal_memory
                        || !self.admits(BuddyZone::Normal, count.data(), 0, level))
            {
                // The reserve prevented trying this zone. This does not claim
                // that bypassing the reserve would find a contiguous block.
                AllocationFailure::Reserve
            } else {
                AllocationFailure::Fragmentation
            };
            self.record_failure(Some(order), reason);
            return Err(reason);
        }
        // debug!(
        //     "buddy_alloc: order = {}, free_addr = {:?}",
        //     order,
        //     free_addr
        // );
        Ok(self.finish_admitted(
            free_addr.unwrap(),
            PageFrameCount::new(1 << (order as usize - MIN_ORDER)),
            level,
        ))
    }

    /// Allocate from a free block whose returned range fits below a physical
    /// address limit. The selected larger buddy block may cross the limit;
    /// only its low split is returned and the remaining halves go back to the
    /// normal free lists.
    pub fn buddy_alloc_below(
        &mut self,
        count: PageFrameCount,
        max_phys_addr: PhysAddr,
    ) -> Option<(PhysAddr, PageFrameCount)> {
        self.buddy_alloc_below_result(count, max_phys_addr).ok()
    }

    fn buddy_alloc_below_result(
        &mut self,
        count: PageFrameCount,
        max_phys_addr: PhysAddr,
    ) -> Result<(PhysAddr, PageFrameCount), AllocationFailure> {
        self.buddy_alloc_below_with_level(count, max_phys_addr, ReserveLevel::Kernel)
    }

    fn buddy_alloc_below_with_level(
        &mut self,
        count: PageFrameCount,
        max_phys_addr: PhysAddr,
        level: ReserveLevel,
    ) -> Result<(PhysAddr, PageFrameCount), AllocationFailure> {
        self.buddy_alloc_below_with_placement(
            count,
            max_phys_addr,
            level,
            FramePlacement::Unmovable,
        )
    }

    fn buddy_alloc_below_with_placement(
        &mut self,
        count: PageFrameCount,
        max_phys_addr: PhysAddr,
        level: ReserveLevel,
        placement: FramePlacement,
    ) -> Result<(PhysAddr, PageFrameCount), AllocationFailure> {
        if max_phys_addr.data() == usize::MAX {
            return self.buddy_alloc_with_placement(count, level, placement);
        }
        let requested_order = self
            .record_request(count, placement)
            .ok_or(AllocationFailure::InvalidOrder)?;
        let requested_bytes = count.data() * A::PAGE_SIZE;
        if max_phys_addr.data() == DMA32_LIMIT {
            let result = self.try_zone(BuddyZone::Dma32, requested_order, level, placement);
            if result.is_err() {
                let reason = if self.lacks_capacity(count) {
                    AllocationFailure::Capacity
                } else if self.dma32_free_pages < count.data() {
                    AllocationFailure::ZoneLimit
                } else if result == Err(AllocationFailure::Reserve)
                    || !self.admits(BuddyZone::Dma32, count.data(), 0, level)
                {
                    AllocationFailure::Reserve
                } else {
                    AllocationFailure::Fragmentation
                };
                self.record_failure(Some(requested_order), reason);
                return Err(reason);
            }
            return Ok(self.finish_admitted(result.unwrap(), count, level));
        }

        let zones: &[BuddyZone] = if max_phys_addr.data() < DMA32_LIMIT {
            &[BuddyZone::Dma32]
        } else {
            &[BuddyZone::Normal, BuddyZone::Dma32]
        };
        let mut admission_denied = false;
        for &zone in zones {
            let index = Self::order2index(requested_order);
            let attempts = &mut self.diagnostics.zones[zone.index()].attempts[index];
            *attempts = attempts.saturating_add(1);
            // A wide User address ceiling restricts representable addresses;
            // it does not authorize consuming the DMA-only fallback reserve.
            // Kernel and genuinely low-address DMA requests retain their own
            // existing admission policy.
            if zone == BuddyZone::Dma32
                && level == ReserveLevel::User
                && max_phys_addr.data() > DMA32_LIMIT
                && !self.dma32_fallback_allowed(count.data())
            {
                admission_denied = true;
                let misses = &mut self.diagnostics.zones[zone.index()].misses[index];
                *misses = misses.saturating_add(1);
                continue;
            }
            for (current_order, class) in placement::allocation_chains(requested_order, placement) {
                let current_order = current_order as usize;
                let mut list_addr = self.class_area(zone, current_order as u8, class);
                if list_addr.is_null() {
                    continue;
                }
                loop {
                    let list: PageList<A> = Self::read_page(list_addr);
                    for index in 0..list.entry_num {
                        let entry = Self::read_entry(list_addr, index);
                        let fits = entry
                            .data()
                            .checked_add(requested_bytes - 1)
                            .is_some_and(|end| end <= max_phys_addr.data());
                        if !fits {
                            continue;
                        }

                        if !self.admits(
                            zone,
                            count.data(),
                            self.split_headroom_for(
                                zone,
                                requested_order,
                                current_order as u8,
                                if current_order >= BLOCK_ORDER {
                                    placement
                                } else {
                                    class
                                },
                            ),
                            level,
                        ) {
                            admission_denied = true;
                            continue;
                        }

                        let base =
                            self.remove_entry(zone, current_order as u8, class, list_addr, index);
                        self.classify_allocation(
                            base,
                            current_order as u8,
                            requested_order,
                            placement,
                        );
                        self.record_placement_fallback(base, placement);
                        let mut split_order = current_order;
                        while split_order > requested_order as usize {
                            split_order -= 1;
                            unsafe {
                                self.insert_without_merge(
                                    base + (1 << split_order),
                                    split_order as u8,
                                )
                            };
                        }
                        return Ok(self.finish_admitted(base, count, level));
                    }
                    if list.next_page.is_null() {
                        break;
                    }
                    list_addr = list.next_page;
                }
            }
            let misses = &mut self.diagnostics.zones[zone.index()].misses[index];
            *misses = misses.saturating_add(1);
        }
        // A limit inside a zone cannot be diagnosed precisely from zone-wide
        // counters. Do not scan free lists again or invent eligible capacity.
        let reason = if self.lacks_capacity(count) {
            AllocationFailure::Capacity
        } else if admission_denied {
            AllocationFailure::Reserve
        } else {
            AllocationFailure::AddressLimit
        };
        self.record_failure(Some(requested_order), reason);
        Err(reason)
    }

    /// 释放一个块
    ///
    /// ## 参数
    ///
    /// - `base` - 块的起始地址
    /// - `order` - 块的阶数
    unsafe fn buddy_free(&mut self, base: PhysAddr, order: u8) {
        self.buddy_free_reported(base, order, false);
    }

    unsafe fn buddy_free_reported(&mut self, mut base: PhysAddr, order: u8, reported: bool) {
        let original_order = order as usize;
        if self.quarantine_free(base, order) {
            return;
        }
        // debug!("buddy_free: base = {:?}, order = {}", base, order);
        let mut order = order as usize;

        while order < MAX_ORDER {
            let zone = Self::zone_for(base, order);
            if zone == BuddyZone::Normal {
                self.has_normal_memory = true;
            }
            // 检测地址是否合法
            if base.data() & ((1 << (order)) - 1) != 0 {
                panic!(
                    "buddy_free: base is not aligned, base = {:#x}, order = {}",
                    base.data(),
                    order
                );
            }

            // 在链表中寻找伙伴块
            // 伙伴块的地址是base ^ (1 << order)
            let buddy_addr = PhysAddr::new(base.data() ^ (1 << order));

            let class = if order < BLOCK_ORDER {
                self.placement.get::<A>(base)
            } else {
                FramePlacement::Unmovable
            };
            let buddy = if order != MAX_ORDER - 1 {
                self.free_entry(buddy_addr).filter(|(buddy_order, _, _)| {
                    *buddy_order as usize == order
                        && (order >= BLOCK_ORDER || self.placement.get::<A>(buddy_addr) == class)
                })
            } else {
                None
            };
            if let Some((_, metadata, slot)) = buddy {
                let removed = self.remove_entry(zone, order as u8, class, metadata, slot);
                debug_assert_eq!(removed, buddy_addr);
            } else {
                self.insert_without_merge(base, order as u8);
                if reported && order == original_order {
                    let head = self.free_area(zone, order as u8);
                    let page: PageList<A> = Self::read_page(head);
                    A::write(
                        Self::entry_virt_addr(head, page.entry_num - 1),
                        PhysAddr::new(base.data() | reporting::REPORTED),
                    );
                }
                return;
            }
            base = min(base, buddy_addr);
            order += 1;
        }
        // 走到这一步，order应该为MAX_ORDER-1
        assert!(order == MAX_ORDER - 1);
    }
}

/// Format only after releasing the frame allocator lock: formatting may itself
/// allocate. This is a DragonOS debug report, not a Linux procfs ABI.
pub(crate) fn allocation_stats_report(snapshot: BuddySnapshot) -> alloc::string::String {
    let mut report = alloc::string::String::new();
    writeln!(
        report,
        "version=1\norder_unit=base_pages\nrequest_unit=buddy_attempt"
    )
    .unwrap();
    for (index, zone) in ["DMA32", "Normal"].iter().enumerate() {
        writeln!(report, "free_pages {zone} {}", snapshot.free_pages[index]).unwrap();
        let stats = &snapshot.diagnostics.zones[index];
        for order in 0..BUDDY_ORDER_COUNT {
            writeln!(
                report,
                "zone {zone} order {order} free_blocks {} attempts {} misses {}",
                stats.free_blocks[order], stats.attempts[order], stats.misses[order],
            )
            .unwrap();
        }
    }
    for order in 0..BUDDY_ORDER_COUNT {
        writeln!(
            report,
            "request order {order} attempts {} failures {}",
            snapshot.diagnostics.requests[order], snapshot.diagnostics.failures[order],
        )
        .unwrap();
    }
    for (name, reason) in FAILURE_REASONS {
        writeln!(
            report,
            "failure {name} {}",
            snapshot.diagnostics.failure_reasons[reason as usize],
        )
        .unwrap();
    }
    for (name, index) in [("unmovable", 0), ("movable", 1)] {
        writeln!(
            report,
            "placement {name} requests {} fallback {}",
            snapshot.diagnostics.placement_requests[index],
            snapshot.diagnostics.placement_fallback[index]
        )
        .unwrap();
    }
    writeln!(
        report,
        "metadata_from_movable={}",
        snapshot.metadata_from_movable
    )
    .unwrap();
    writeln!(report, "snapshot_end=1").unwrap();
    report
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
pub(crate) fn allocation_diagnostics_selftest_report() -> alloc::string::String {
    let (bounded, split_merge, fragmentation, zones, metadata) = deterministic_buddy_selftest();
    let checks = [
        ("bounded_selection", bounded),
        ("split_merge", split_merge),
        ("fragmentation_and_capacity_diagnostics", fragmentation),
        ("zone_reserve_and_address_diagnostics", zones),
        ("metadata_count_conservation", metadata),
        ("reserve_scope_irq_fork", super::reserve::selftest()),
    ];
    let failures = checks.iter().filter(|(_, passed)| !passed).count();
    let mut report = alloc::string::String::new();
    writeln!(
        report,
        "status={}",
        if failures == 0 { "ok" } else { "fail" }
    )
    .unwrap();
    for (name, passed) in checks {
        writeln!(report, "{name}={}", if passed { "ok" } else { "fail" }).unwrap();
    }
    writeln!(report, "summary_fail={failures}").unwrap();
    report
}

#[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
pub(crate) fn placement_selftest_report() -> alloc::string::String {
    placement::selftest_report()
}

/// Exercise bounded allocation, splitting, and merging on a private arena.
///
/// The live allocator only supplies one aligned backing block.  All operations
/// under test use a separate `BuddyAllocator`, so their exact addresses are
/// deterministic and concurrent kernel allocations cannot affect the result.
#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
pub(crate) fn deterministic_buddy_selftest() -> (bool, bool, bool, bool, bool) {
    const BACKING_PAGES: usize = 128;
    const ARENA_OFFSET_PAGES: usize = 64;
    const ARENA_PAGES: usize = 16;

    fn counts_match(allocator: &BuddyAllocator<MMArch>) -> bool {
        let snapshot = allocator.diagnostic_snapshot();
        snapshot
            .diagnostics
            .zones
            .iter()
            .enumerate()
            .all(|(index, zone)| {
                let pages: usize = zone
                    .free_blocks
                    .iter()
                    .enumerate()
                    .map(|(order, blocks)| blocks << order)
                    .sum();
                pages == snapshot.free_pages[index]
            })
    }

    let Some((backing, backing_count)) =
        (unsafe { allocate_page_frames(PageFrameCount::new(BACKING_PAGES)) })
    else {
        return (false, false, false, false, false);
    };

    unsafe fn empty_isolated_allocator(backing: PhysAddr) -> BuddyAllocator<MMArch> {
        let dma32_free_area = [PhysAddr::new(0); MAX_ORDER - MIN_ORDER];
        let normal_free_area = [PhysAddr::new(0); MAX_ORDER - MIN_ORDER];
        let mut dma32_empty_area = [PhysAddr::new(0); MAX_ORDER - MIN_ORDER];
        let mut normal_empty_area = [PhysAddr::new(0); MAX_ORDER - MIN_ORDER];
        for (index, area) in dma32_empty_area
            .iter_mut()
            .chain(normal_empty_area.iter_mut())
            .enumerate()
        {
            *area = backing + index * MMArch::PAGE_SIZE;
            core::ptr::write_bytes(
                MMArch::phys_2_virt(*area).unwrap().as_ptr::<u8>(),
                0,
                MMArch::PAGE_SIZE,
            );
            BuddyAllocator::<MMArch>::write_page(*area, PageList::new(0, PhysAddr::new(0)));
        }
        BuddyAllocator {
            dma32_free_area,
            normal_free_area,
            movable_free_area: [[PhysAddr::new(0); LOW_ORDERS]; 2],
            movable_blocks: [[0; LOW_ORDERS]; 2],
            placement: PlacementMap::empty(),
            // Watermark split tests additionally publish sparse order-0
            // identities beyond the backed arena. They are index keys only;
            // metadata and the source extent remain within BACKING_PAGES.
            free_index: BuddyAllocator::<MMArch>::fixture_index(
                backing,
                BACKING_PAGES
                    .max(ARENA_OFFSET_PAGES + 2 * BuddyAllocator::<MMArch>::BUDDY_ENTRIES + 2),
            ),
            metadata_from_movable: 0,
            dma32_empty_area,
            normal_empty_area,
            dma32_free_pages: 0,
            normal_free_pages: 0,
            diagnostics: BuddyDiagnostics::default(),
            isolation: None,
            next_isolation_id: 1,
            reporting: reporting::ReportingState::new(),
            has_normal_memory: false,
            total: PageFrameCount::new(ARENA_PAGES),
            watermarks: [ZoneWatermarks::default(); 2],
            phantom: PhantomData,
        }
    }

    unsafe fn isolated_allocator(backing: PhysAddr) -> BuddyAllocator<MMArch> {
        let mut allocator = empty_isolated_allocator(backing);
        allocator.buddy_free(
            backing + ARENA_OFFSET_PAGES * MMArch::PAGE_SIZE,
            (MIN_ORDER + ARENA_PAGES.trailing_zeros() as usize) as u8,
        );
        allocator
    }

    let arena = backing + ARENA_OFFSET_PAGES * MMArch::PAGE_SIZE;
    let arena_max = arena + ARENA_PAGES * MMArch::PAGE_SIZE - 1;

    let bounded_selection_ok = {
        let mut allocator = unsafe { empty_isolated_allocator(backing) };
        let low = arena;
        let high = arena + ARENA_PAGES * MMArch::PAGE_SIZE;
        // Insert the out-of-mask block first so the bounded search has to skip
        // it rather than succeeding accidentally on the first entry.
        unsafe {
            allocator.free(high, PageFrameCount::new(8));
            allocator.free(low, PageFrameCount::new(8));
        }
        let low_max = low + 8 * MMArch::PAGE_SIZE - 1;
        let selected = allocator.buddy_alloc_below(PageFrameCount::new(8), low_max);
        let remaining =
            allocator.buddy_alloc_below(PageFrameCount::new(8), high + 8 * MMArch::PAGE_SIZE - 1);
        selected.is_some_and(|(base, count)| base == low && count.data() == 8)
            && remaining.is_some_and(|(base, count)| base == high && count.data() == 8)
    };

    let split_merge_ok = {
        let mut allocator = unsafe { isolated_allocator(backing) };
        let first = allocator.buddy_alloc_below(PageFrameCount::new(8), arena_max);
        let second = allocator.buddy_alloc_below(PageFrameCount::new(8), arena_max);
        match (first, second) {
            (Some((first, first_count)), Some((second, second_count))) => {
                let exact_split = first_count.data() == 8
                    && second_count.data() == 8
                    && min(first, second) == arena
                    && core::cmp::max(first, second) == arena + 8 * MMArch::PAGE_SIZE;
                unsafe {
                    allocator.free(first, first_count);
                    allocator.free(second, second_count);
                }
                let merged = allocator.buddy_alloc_below(PageFrameCount::new(16), arena_max);
                exact_split
                    && merged.is_some_and(|(base, count)| base == arena && count.data() == 16)
                    && counts_match(&allocator)
            }
            _ => false,
        }
    };

    let fragmented_ok = {
        let mut allocator = unsafe { isolated_allocator(backing) };
        let mut pages = alloc::vec::Vec::with_capacity(ARENA_PAGES);
        for _ in 0..ARENA_PAGES {
            let Some((base, count)) = allocator.buddy_alloc_below(PageFrameCount::ONE, arena_max)
            else {
                break;
            };
            if count != PageFrameCount::ONE {
                break;
            }
            pages.push(base);
        }
        pages.sort_unstable();
        let exact_pages = pages.len() == ARENA_PAGES
            && pages
                .iter()
                .enumerate()
                .all(|(index, base)| *base == arena + index * MMArch::PAGE_SIZE);

        if exact_pages {
            for index in (0..ARENA_PAGES).step_by(2) {
                unsafe { allocator.free(pages[index], PageFrameCount::ONE) };
            }
            // The backing arena may be in Normal memory on large guests.
            // Use an unrestricted request there so the expected diagnosis is
            // fragmentation rather than a DMA32 address restriction.
            let arena_zone = BuddyAllocator::<MMArch>::zone_for(arena, MIN_ORDER).index();
            let fragmented_rejects_pair = if arena_zone == BuddyZone::Normal.index() {
                allocator.buddy_alloc(PageFrameCount::new(2)).is_none()
            } else {
                allocator
                    .buddy_alloc_below(PageFrameCount::new(2), PhysAddr::new(DMA32_LIMIT))
                    .is_none()
            };
            let snapshot = allocator.diagnostic_snapshot();
            let fragmentation_counted = snapshot.free_pages[arena_zone] == ARENA_PAGES / 2
                && snapshot.diagnostics.zones[arena_zone].free_blocks[0] == ARENA_PAGES / 2
                && snapshot.diagnostics.failure_reasons[AllocationFailure::Fragmentation as usize]
                    == 1
                && counts_match(&allocator);

            unsafe { allocator.free(pages[1], PageFrameCount::ONE) };
            let pair = allocator.buddy_alloc_below(PageFrameCount::new(2), arena_max);
            let exact_pair = pair.is_some_and(|(base, count)| base == arena && count.data() == 2);
            if let Some((base, count)) = pair {
                unsafe { allocator.free(base, count) };
            }
            for index in (3..ARENA_PAGES).step_by(2) {
                unsafe { allocator.free(pages[index], PageFrameCount::ONE) };
            }
            let merged = allocator.buddy_alloc_below(PageFrameCount::new(16), arena_max);
            let capacity_rejected = allocator.buddy_alloc(PageFrameCount::ONE).is_none();
            let invalid_rejected = allocator
                .buddy_alloc(PageFrameCount::new(1 << BUDDY_ORDER_COUNT))
                .is_none();
            let snapshot = allocator.diagnostic_snapshot();
            fragmented_rejects_pair
                && fragmentation_counted
                && exact_pair
                && merged.is_some_and(|(base, count)| base == arena && count.data() == 16)
                && capacity_rejected
                && invalid_rejected
                && snapshot.diagnostics.failure_reasons[AllocationFailure::Capacity as usize] == 1
                && snapshot.diagnostics.failure_reasons[AllocationFailure::InvalidOrder as usize]
                    == 1
                && counts_match(&allocator)
        } else {
            false
        }
    };

    let dma32_zone_ok = {
        let mut allocator = unsafe { empty_isolated_allocator(backing) };
        let count = PageFrameCount::new(8);
        let bytes = count.data() * MMArch::PAGE_SIZE;
        let low = PhysAddr::new((1usize << 32) - bytes);
        let high = PhysAddr::new(1usize << 32);
        unsafe {
            allocator.free(high, count);
            allocator.free(low, count);
        }
        let zone_limit_rejected = allocator
            .buddy_alloc_below(PageFrameCount::new(16), PhysAddr::new(DMA32_LIMIT))
            .is_none();
        let address_limit_rejected = allocator
            .buddy_alloc_below(count, low + 4 * MMArch::PAGE_SIZE - 1)
            .is_none();
        let snapshot = allocator.diagnostic_snapshot();
        let restriction_diagnostics_ok = zone_limit_rejected
            && address_limit_rejected
            && snapshot.diagnostics.failure_reasons[AllocationFailure::ZoneLimit as usize] == 1
            && snapshot.diagnostics.failure_reasons[AllocationFailure::AddressLimit as usize] == 1
            && counts_match(&allocator);
        let dma32 = allocator.buddy_alloc_below(count, PhysAddr::new(DMA32_LIMIT));
        let normal = unsafe { allocator.allocate(count) };
        let selection_ok = dma32.is_some_and(|(base, allocated)| base == low && allocated == count)
            && normal.is_some_and(|(base, allocated)| base == high && allocated == count);

        let mut reserve_allocator = unsafe { empty_isolated_allocator(backing) };
        let reserve_low = PhysAddr::new(2usize << 30);
        let reserve_count = PageFrameCount::new(DMA32_FALLBACK_RESERVE_PAGES);
        let one_high = PhysAddr::new(1usize << 32);
        unsafe {
            reserve_allocator.free(reserve_low, reserve_count);
            reserve_allocator.free(one_high, PageFrameCount::ONE);
        }
        let normal_first = unsafe { reserve_allocator.allocate(PageFrameCount::ONE) };
        let reserve_blocks_fallback = unsafe { reserve_allocator.allocate(PageFrameCount::ONE) };
        let dma_still_available =
            reserve_allocator.buddy_alloc_below(PageFrameCount::ONE, PhysAddr::new(DMA32_LIMIT));
        let reserve_ok = normal_first.is_some_and(|(base, _)| base == one_high)
            && reserve_blocks_fallback.is_none()
            && dma_still_available.is_some_and(|(base, _)| base == reserve_low)
            && reserve_allocator.diagnostics.failure_reasons[AllocationFailure::Reserve as usize]
                == 1
            && counts_match(&reserve_allocator);

        selection_ok && reserve_ok && restriction_diagnostics_ok
    };

    let typed_dma_ok =
        recovery::typed_allocation_selftest(|| unsafe { empty_isolated_allocator(backing) });

    let metadata_reuse_ok = {
        let mut allocator = unsafe { empty_isolated_allocator(backing) };
        let list_pages = [
            backing + 40 * MMArch::PAGE_SIZE,
            backing + 41 * MMArch::PAGE_SIZE,
            backing + 42 * MMArch::PAGE_SIZE,
        ];
        for (page_index, &page) in list_pages.iter().enumerate() {
            let next = list_pages
                .get(page_index + 1)
                .copied()
                .unwrap_or(PhysAddr::new(0));
            BuddyAllocator::<MMArch>::write_page(
                page,
                PageList::new(BuddyAllocator::<MMArch>::BUDDY_ENTRIES, next)
                    .with_order(MIN_ORDER as u8),
            );
            for entry_index in 0..BuddyAllocator::<MMArch>::BUDDY_ENTRIES {
                let ordinal = page_index * BuddyAllocator::<MMArch>::BUDDY_ENTRIES + entry_index;
                unsafe {
                    MMArch::write(
                        BuddyAllocator::<MMArch>::entry_virt_addr(page, entry_index),
                        PhysAddr::new(0x1000_0000 + ordinal * 2 * MMArch::PAGE_SIZE),
                    );
                }
                let address = PhysAddr::new(0x1000_0000 + ordinal * 2 * MMArch::PAGE_SIZE);
                allocator.free_index.insert(
                    address,
                    BuddyAllocator::<MMArch>::entry_addr(page, entry_index),
                    MIN_ORDER as u8,
                );
            }
        }
        allocator.dma32_free_area[0] = list_pages[0];
        allocator.dma32_free_pages = list_pages.len() * BuddyAllocator::<MMArch>::BUDDY_ENTRIES;
        allocator.diagnostics.zones[BuddyZone::Dma32.index()].free_blocks[0] =
            allocator.dma32_free_pages;

        let mut drained = 0;
        while allocator
            .pop_front(BuddyZone::Dma32, MIN_ORDER as u8)
            .is_some()
        {
            drained += 1;
        }
        let expected = list_pages.len() * BuddyAllocator::<MMArch>::BUDDY_ENTRIES;
        let first_drain_ok = drained == expected
            && allocator.dma32_free_area[0].is_null()
            && allocator.dma32_free_pages == 0;

        for index in 0..16 {
            unsafe {
                allocator.insert_without_merge(
                    PhysAddr::new(0x3000_0000 + index * 2 * MMArch::PAGE_SIZE),
                    MIN_ORDER as u8,
                );
            }
        }
        let mut redrained = 0;
        while allocator
            .pop_front(BuddyZone::Dma32, MIN_ORDER as u8)
            .is_some()
        {
            redrained += 1;
        }
        first_drain_ok
            && redrained == 16
            && allocator.dma32_free_area[0].is_null()
            && allocator.dma32_free_pages == 0
            && counts_match(&allocator)
    };

    let watermarks_ok = watermark_selftest(|| unsafe { empty_isolated_allocator(backing) }, arena);
    unsafe {
        deallocate_page_frames(PhysPageFrame::new(backing), backing_count);
    }
    (
        bounded_selection_ok && watermarks_ok,
        split_merge_ok,
        fragmented_ok,
        dma32_zone_ok && typed_dma_ok,
        metadata_reuse_ok,
    )
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
fn watermark_selftest(fresh: impl Fn() -> BuddyAllocator<MMArch>, arena: PhysAddr) -> bool {
    let mut allocator = fresh();
    let low = PhysAddr::new(0x0200_0000);
    let high = PhysAddr::new(1usize << 32);
    unsafe {
        allocator.free(low, PageFrameCount::new(8));
        allocator.free(high, PageFrameCount::new(8));
    }
    allocator.watermarks = [ZoneWatermarks {
        min: 2,
        low: 4,
        high: 6,
    }; 2];
    let max = PhysAddr::new(DMA32_LIMIT);
    let allocate = |allocator: &mut BuddyAllocator<MMArch>, count, level| {
        allocator
            .buddy_alloc_below_with_level(PageFrameCount::new(count), max, level)
            .is_ok()
    };
    let first = allocate(&mut allocator, 4, ReserveLevel::User);
    let before = allocator.dma32_free_pages;
    let denied =
        !allocate(&mut allocator, 1, ReserveLevel::User) && allocator.dma32_free_pages == before;
    let kernel = allocate(&mut allocator, 1, ReserveLevel::Kernel)
        && allocate(&mut allocator, 1, ReserveLevel::Kernel)
        && !allocate(&mut allocator, 1, ReserveLevel::Kernel);
    let half = allocate(&mut allocator, 1, ReserveLevel::OomExit)
        && !allocate(&mut allocator, 1, ReserveLevel::OomExit);
    let handoff = !allocator.held_handoff_allowed(low, ReserveLevel::Kernel)
        && allocator.held_handoff_allowed(low, ReserveLevel::OomExit)
        && allocator.held_handoff_allowed(high, ReserveLevel::Kernel);
    let full = allocate(&mut allocator, 1, ReserveLevel::Reclaim)
        && allocator.dma32_free_pages == 0
        && allocator.normal_free_pages == 8;
    let pressure = allocator.pressure_snapshot();
    let zone = pressure.needs_reclaim(low) && !pressure.needs_reclaim(high);

    let mut split = fresh();
    // A finite mask skips a full order-0 list and splits the low order-1
    // source. Its right page really becomes metadata, inside our owned arena.
    // Sparse out-of-mask frame identities are never dereferenced or returned.
    let source_zone = BuddyAllocator::<MMArch>::zone_for(arena, MIN_ORDER + 1);
    let outside = arena + 2 * MMArch::PAGE_SIZE;
    let last = outside + 2 * (BuddyAllocator::<MMArch>::BUDDY_ENTRIES - 1) * MMArch::PAGE_SIZE;
    if BuddyAllocator::<MMArch>::zone_for(last, MIN_ORDER) != source_zone {
        return false;
    }
    unsafe {
        for index in 0..BuddyAllocator::<MMArch>::BUDDY_ENTRIES {
            split.insert_without_merge(outside + 2 * index * MMArch::PAGE_SIZE, MIN_ORDER as u8);
        }
        split.free(arena, PageFrameCount::new(2));
    }
    let free = split.zone_free(source_zone);
    split.watermarks[source_zone.index()] = ZoneWatermarks {
        min: free - 1,
        low: free - 1,
        high: free - 1,
    };
    let headroom =
        split.split_metadata_headroom(source_zone, MIN_ORDER as u8, (MIN_ORDER + 1) as u8);
    let limit = arena + MMArch::PAGE_SIZE - 1;
    let before = split.diagnostic_snapshot();
    let head = split.free_area(source_zone, MIN_ORDER as u8);
    let head_entries = BuddyAllocator::<MMArch>::read_page::<PageList<MMArch>>(head).entry_num;
    let metadata_denial =
        split.buddy_alloc_below_with_level(PageFrameCount::ONE, limit, ReserveLevel::Kernel)
            == Err(AllocationFailure::Reserve)
            && split.diagnostic_snapshot().free_pages == before.free_pages
            && split.free_area(source_zone, MIN_ORDER as u8) == head
            && BuddyAllocator::<MMArch>::read_page::<PageList<MMArch>>(head).entry_num
                == head_entries
            && split.diagnostic_snapshot().diagnostics.zones[source_zone.index()].free_blocks
                == before.diagnostics.zones[source_zone.index()].free_blocks;
    split.watermarks[source_zone.index()].min = free - 2;
    let metadata_boundary =
        split.buddy_alloc_below_with_level(PageFrameCount::ONE, limit, ReserveLevel::Kernel)
            == Ok((arena, PageFrameCount::ONE))
            && split.zone_free(source_zone) == free - 2;
    let marks = super::watermark::calculate([262144, 0], 4096);
    first
        && denied
        && kernel
        && half
        && handoff
        && full
        && zone
        && metadata_denial
        && headroom == 1
        && metadata_boundary
        && marks[0].min == 1024
        && marks[0].low == 1286
        && marks[0].high == 1548
        && marks[1].high == 0
}

impl<A: MemoryManagementArch> FrameAllocator for BuddyAllocator<A> {
    unsafe fn allocate_request(
        &mut self,
        request: super::page_frame::FrameAllocationRequest,
    ) -> Option<(PhysAddr, PageFrameCount)> {
        self.allocate_admitted(request, request.reserve_level())
    }
    unsafe fn allocate(&mut self, count: PageFrameCount) -> Option<(PhysAddr, PageFrameCount)> {
        return self.buddy_alloc(count);
    }

    /// 释放一个块
    ///
    /// ## 参数
    ///
    /// - `base` - 块的起始地址
    /// - `count` - 块的页数（必须是2的幂）
    ///
    /// ## Panic
    ///
    /// 如果count不是2的幂，会panic
    unsafe fn free(&mut self, base: PhysAddr, count: PageFrameCount) {
        // 要求count是2的幂
        if unlikely(!count.data().is_power_of_two()) {
            warn!("buddy free: count is not power of two");
        }
        let mut order = log2(count.data());
        if count.data() & ((1 << order) - 1) != 0 {
            order += 1;
        }
        let order = (order + MIN_ORDER) as u8;
        // debug!("free: base={:?}, count={:?}", base, count);
        self.buddy_free(base, order);
    }

    unsafe fn usage(&self) -> PageFrameUsage {
        let free = PageFrameCount::new(self.dma32_free_pages + self.normal_free_pages);
        PageFrameUsage::new(self.total - free, self.total)
    }
}

/// 一个用于计算整数的对数的函数，会向下取整。（由于内核不能进行浮点运算，因此需要这个函数）
fn log2(x: usize) -> usize {
    let leading_zeros = x.leading_zeros() as usize;
    let log2x = 63 - leading_zeros;
    return log2x;
}

/// Runs a synchronous mapping test against real, isolated fragmented RAM.
/// Donated frames are thereafter owned (and freed globally) by VmAllocation.
#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
pub(crate) fn with_fragmented_buffer_arena(
    test: impl FnOnce(&mut dyn FnMut() -> Result<PhysAddr, system_error::SystemError>) -> bool,
) -> bool {
    use crate::arch::mm::LockedFrameAllocator;
    const BACKING: usize = 128;
    const OFFSET: usize = 64;
    const PAGES: usize = 16;
    struct Backing {
        base: PhysAddr,
        donated: [bool; BACKING],
    }
    impl Drop for Backing {
        fn drop(&mut self) {
            for index in 0..BACKING {
                if !self.donated[index] {
                    unsafe { LockedFrameAllocator.free_one(self.base + index * MMArch::PAGE_SIZE) };
                }
            }
        }
    }
    let Some((base, count)) = (unsafe {
        LockedFrameAllocator.allocate_below(PageFrameCount::new(BACKING), PhysAddr::new(usize::MAX))
    }) else {
        return false;
    };
    assert_eq!(count.data(), BACKING);
    let mut backing = Backing {
        base,
        donated: [false; BACKING],
    };
    let mut dma32_empty_area = [PhysAddr::new(0); BUDDY_ORDER_COUNT];
    let mut normal_empty_area = [PhysAddr::new(0); BUDDY_ORDER_COUNT];
    for (index, area) in dma32_empty_area
        .iter_mut()
        .chain(normal_empty_area.iter_mut())
        .enumerate()
    {
        *area = base + index * MMArch::PAGE_SIZE;
        unsafe {
            core::ptr::write_bytes(
                MMArch::phys_2_virt(*area).unwrap().as_ptr::<u8>(),
                0,
                MMArch::PAGE_SIZE,
            );
            BuddyAllocator::<MMArch>::write_page(*area, PageList::new(0, PhysAddr::new(0)));
        }
    }
    let mut allocator = BuddyAllocator::<MMArch> {
        dma32_free_area: [PhysAddr::new(0); BUDDY_ORDER_COUNT],
        normal_free_area: [PhysAddr::new(0); BUDDY_ORDER_COUNT],
        movable_free_area: [[PhysAddr::new(0); LOW_ORDERS]; 2],
        movable_blocks: [[0; LOW_ORDERS]; 2],
        placement: PlacementMap::empty(),
        free_index: BuddyAllocator::<MMArch>::fixture_index(base, BACKING),
        metadata_from_movable: 0,
        dma32_empty_area,
        normal_empty_area,
        dma32_free_pages: 0,
        normal_free_pages: 0,
        diagnostics: BuddyDiagnostics::default(),
        isolation: None,
        next_isolation_id: 1,
        reporting: reporting::ReportingState::new(),
        has_normal_memory: false,
        total: PageFrameCount::new(PAGES),
        watermarks: [ZoneWatermarks::default(); 2],
        phantom: PhantomData,
    };
    // Odd pages remain owned by backing; only isolated even pages are free.
    for index in (0..PAGES).step_by(2) {
        unsafe {
            allocator.free(
                base + (OFFSET + index) * MMArch::PAGE_SIZE,
                PageFrameCount::ONE,
            )
        };
    }
    let max = PhysAddr::new(usize::MAX);
    if allocator
        .buddy_alloc_below(PageFrameCount::new(2), max)
        .is_some()
    {
        return false;
    }
    let snapshot = allocator.diagnostic_snapshot();
    if snapshot.free_pages.iter().sum::<usize>() != PAGES / 2 {
        return false;
    }
    // The closure must destroy its VmAllocation before returning. No private
    // allocator access follows it: donated frames may already be globally reused.
    test(&mut || {
        let (frame, count) = allocator
            .buddy_alloc_below(PageFrameCount::ONE, max)
            .ok_or(system_error::SystemError::ENOMEM)?;
        assert_eq!(count, PageFrameCount::ONE);
        let index = (frame.data() - base.data()) / MMArch::PAGE_SIZE;
        assert!((OFFSET..OFFSET + PAGES).contains(&index) && index.is_multiple_of(2));
        backing.donated[index] = true;
        unsafe {
            core::ptr::write_bytes(
                MMArch::phys_2_virt(frame).unwrap().as_ptr::<u8>(),
                0,
                MMArch::PAGE_SIZE,
            )
        };
        Ok(frame)
    })
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
pub(crate) use reporting::selftest_report as reporting_selftest_report;
