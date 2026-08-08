use log::{debug, warn};

/// @Author: longjin@dragonos.org
/// @Author: kongweichao@dragonos.org
/// @Date: 2023-03-28 16:03:47
/// @FilePath: /DragonOS/kernel/src/mm/allocator/buddy.rs
/// @Description: 伙伴分配器
use crate::arch::MMArch;
use crate::mm::allocator::bump::BumpAllocator;
#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
use crate::mm::allocator::page_frame::{
    allocate_page_frames, deallocate_page_frames, PhysPageFrame,
};
use crate::mm::allocator::page_frame::{FrameAllocator, PageFrameCount, PageFrameUsage};
use crate::mm::{MemoryManagementArch, PhysAddr, PhysMemoryArea, VirtAddr};

use core::cmp::min;
use core::fmt::Debug;
use core::intrinsics::{likely, unlikely};

use core::{marker::PhantomData, mem};

// 一个全局变量MAX_ORDER，用来表示buddy算法的最大阶数 [MIN_ORDER, MAX_ORDER)左闭右开区间
const MAX_ORDER: usize = 31;
// 4KB
const MIN_ORDER: usize = 12;
const DMA32_LIMIT: usize = u32::MAX as usize;
/// Keep 16 MiB of low memory available for devices which cannot address
/// Normal memory when an unrestricted allocation falls back to DMA32.
const DMA32_FALLBACK_RESERVE_PAGES: usize = (16 * 1024 * 1024) >> MIN_ORDER;
const DMA32_ALLOCATION_METADATA_HEADROOM: usize = MAX_ORDER - MIN_ORDER;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuddyZone {
    Dma32,
    Normal,
}

/// 保存buddy算法中每一页存放的BuddyEntry的信息，占据每个页的起始位置
#[derive(Debug)]
pub struct PageList<A> {
    // 页存放entry的数量
    entry_num: usize,
    // 下一个页面的地址
    next_page: PhysAddr,
    phantom: PhantomData<A>,
}

impl<A> Clone for PageList<A> {
    fn clone(&self) -> Self {
        Self {
            entry_num: self.entry_num,
            next_page: self.next_page,
            phantom: PhantomData,
        }
    }
}

impl<A> PageList<A> {
    #[allow(dead_code)]
    fn empty() -> Self {
        Self {
            entry_num: 0,
            next_page: PhysAddr::new(0),
            phantom: PhantomData,
        }
    }
    fn new(entry_num: usize, next_page: PhysAddr) -> Self {
        Self {
            entry_num,
            next_page,
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
    // DMA32 and normal memory have separate free lists.  This keeps a
    // 32-bit DMA allocation bounded by the number of buddy orders instead of
    // scanning every high-memory free block while interrupts are disabled.
    dma32_free_area: [PhysAddr; MAX_ORDER - MIN_ORDER],
    normal_free_area: [PhysAddr; MAX_ORDER - MIN_ORDER],
    dma32_empty_area: [PhysAddr; MAX_ORDER - MIN_ORDER],
    normal_empty_area: [PhysAddr; MAX_ORDER - MIN_ORDER],
    dma32_free_pages: usize,
    normal_free_pages: usize,
    /// Whether this allocator has ever managed memory above 4 GiB.  Most
    /// current systems are DMA32-only; avoid probing every empty Normal order
    /// on their hot allocation path.
    has_normal_memory: bool,
    /// 总页数
    total: PageFrameCount,
    phantom: PhantomData<A>,
}

impl<A: MemoryManagementArch> BuddyAllocator<A> {
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

        let mut allocator = Self {
            dma32_free_area,
            normal_free_area,
            dma32_empty_area,
            normal_empty_area,
            dma32_free_pages: 0,
            normal_free_pages: 0,
            has_normal_memory: false,
            total: PageFrameCount::new(0),
            phantom: PhantomData,
        };

        let mut total_pages_to_buddy = PageFrameCount::new(0);
        static mut RES_AREAS: [PhysMemoryArea; 128] = [PhysMemoryArea::DEFAULT; 128];
        let mut offset_in_remain_area = bump_allocator
            .remain_areas(&mut RES_AREAS)
            .expect("BuddyAllocator: failed to get remain areas from bump allocator");

        let remain_areas = &RES_AREAS[0..];

        for area in remain_areas {
            let mut paddr = (area.area_base_aligned() + offset_in_remain_area).data();
            let mut remain_pages =
                PageFrameCount::from_bytes(area.area_end_aligned().data() - paddr).unwrap();

            if remain_pages.data() == 0 {
                continue;
            }
            // debug!("area: {area:?}, paddr: {paddr:#x}, remain_pages: {remain_pages:?}");

            total_pages_to_buddy += remain_pages;

            if offset_in_remain_area != 0 {
                offset_in_remain_area = 0;
            }

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

    pub fn write_page(curr_page: PhysAddr, page_list: PageList<A>) {
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
        let index = Self::order2index(order);
        match zone {
            BuddyZone::Dma32 => self.dma32_free_area[index],
            BuddyZone::Normal => self.normal_free_area[index],
        }
    }

    #[inline]
    fn set_free_area(&mut self, zone: BuddyZone, order: u8, value: PhysAddr) {
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

    fn recycle_empty_metadata(
        &mut self,
        zone: BuddyZone,
        order: u8,
        page_addr: PhysAddr,
        next_nonempty: PhysAddr,
    ) {
        debug_assert_eq!(self.free_area(zone, order), page_addr);
        self.set_free_area(zone, order, next_nonempty);
        let empty_head = self.empty_area(zone, order);
        Self::write_page(page_addr, PageList::new(0, empty_head));
        self.set_empty_area(zone, order, page_addr);
    }

    /// Remove one entry from the non-empty chain head.  If the head becomes
    /// empty it is unlinked immediately and moved to the reusable metadata
    /// chain, so allocation never walks historical empty pages.
    fn remove_entry(
        &mut self,
        zone: BuddyZone,
        order: u8,
        target_page_addr: PhysAddr,
        target_index: usize,
    ) -> PhysAddr {
        let mut target_page: PageList<A> = Self::read_page(target_page_addr);
        assert!(target_index < target_page.entry_num);
        let removed: PhysAddr =
            unsafe { A::read(Self::entry_virt_addr(target_page_addr, target_index)) };
        let first_addr = self.free_area(zone, order);
        assert!(!first_addr.is_null());
        let mut first: PageList<A> = Self::read_page(first_addr);
        assert!(first.entry_num > 0);
        let last_index = first.entry_num - 1;
        let last: PhysAddr = unsafe { A::read(Self::entry_virt_addr(first_addr, last_index)) };

        if target_page_addr == first_addr {
            if target_index != last_index {
                unsafe {
                    A::write(Self::entry_virt_addr(first_addr, target_index), last);
                }
            }
            first.entry_num -= 1;
            unsafe {
                A::write(
                    Self::entry_virt_addr(first_addr, first.entry_num),
                    PhysAddr::new(0),
                );
            }
            let next_nonempty = first.next_page;
            Self::write_page(first_addr, first);
            if last_index == 0 {
                self.recycle_empty_metadata(zone, order, first_addr, next_nonempty);
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
            let next_nonempty = first.next_page;
            Self::write_page(first_addr, first);
            if last_index == 0 {
                self.recycle_empty_metadata(zone, order, first_addr, next_nonempty);
            }
            // The target page remains non-empty because the removed slot was
            // replaced from the first non-empty page.
            target_page = Self::read_page(target_page_addr);
            debug_assert!(target_page.entry_num > 0);
        }

        self.adjust_free_pages(zone, 1usize << (order as usize - MIN_ORDER), false);
        removed
    }

    /// Insert a block whose buddy is known to be allocated.  This is used for
    /// halves produced by an allocation split, avoiding a pointless linear
    /// buddy search on the IRQ-off allocation path.
    unsafe fn insert_without_merge(&mut self, base: PhysAddr, order: u8) {
        let zone = Self::zone_for(base, order as usize);
        if zone == BuddyZone::Normal {
            self.has_normal_memory = true;
        }
        let mut first_addr = self.free_area(zone, order);
        let needs_metadata = first_addr.is_null()
            || Self::read_page::<PageList<A>>(first_addr).entry_num == Self::BUDDY_ENTRIES;

        if needs_metadata {
            let empty_head = self.empty_area(zone, order);
            let new_page_addr = if !empty_head.is_null() {
                let empty: PageList<A> = Self::read_page(empty_head);
                self.set_empty_area(zone, order, empty.next_page);
                empty_head
            } else if order as usize == MIN_ORDER {
                base
            } else {
                // The current order is full, so consume one block from this
                // same order as metadata and return all but its first page to
                // strictly lower orders.  This keeps metadata recursion
                // bounded and never searches/merges on the allocation path.
                let metadata_base = self
                    .pop_front(zone, order)
                    .expect("full buddy order has no metadata source");
                let mut split_order = order as usize;
                while split_order > MIN_ORDER {
                    split_order -= 1;
                    self.insert_without_merge(
                        metadata_base + (1 << split_order),
                        split_order as u8,
                    );
                }
                metadata_base
            };
            core::ptr::write_bytes(
                A::phys_2_virt(new_page_addr).unwrap().as_ptr::<u8>(),
                0,
                A::PAGE_SIZE,
            );
            if new_page_addr == base {
                let empty_head = self.empty_area(zone, order);
                Self::write_page(new_page_addr, PageList::new(0, empty_head));
                self.set_empty_area(zone, order, new_page_addr);
                return;
            }
            Self::write_page(new_page_addr, PageList::new(0, first_addr));
            self.set_free_area(zone, order, new_page_addr);
            first_addr = new_page_addr;
        }

        let mut first = Self::read_page::<PageList<A>>(first_addr);
        debug_assert!(first.entry_num < Self::BUDDY_ENTRIES);
        A::write(Self::entry_virt_addr(first_addr, first.entry_num), base);
        first.entry_num += 1;
        Self::write_page(first_addr, first);
        self.adjust_free_pages(zone, 1usize << (order as usize - MIN_ORDER), true);
    }

    /// 从空闲链表的开头，取出1个指定阶数的伙伴块，如果没有，则返回None
    ///
    /// ## 参数
    ///
    /// - `order` - 伙伴块的阶数
    fn pop_front(&mut self, zone: BuddyZone, order: u8) -> Option<PhysAddr> {
        let mut alloc_in_specific_order = |spec_order: u8| {
            let page_list_addr = self.free_area(zone, spec_order);
            if page_list_addr.is_null() {
                return None;
            }
            let page_list: PageList<A> = Self::read_page(page_list_addr);
            let entry =
                self.remove_entry(zone, spec_order, page_list_addr, page_list.entry_num - 1);
            assert!(!entry.is_null());
            assert!(entry.check_aligned(1 << spec_order));
            Some(entry)
        };
        let result: Option<PhysAddr> = alloc_in_specific_order(order);
        // debug!("result={:?}", result);
        if result.is_some() {
            return result;
        }
        // 尝试从更大的链表中分裂

        let mut current_order = (order + 1) as usize;
        let mut x: Option<PhysAddr> = None;
        while current_order < MAX_ORDER {
            x = alloc_in_specific_order(current_order as u8);
            // debug!("current_order={:?}", current_order);
            if x.is_some() {
                break;
            }
            current_order += 1;
        }

        // debug!("x={:?}", x);
        // 如果找到一个大的块，就进行分裂
        if let Some(x) = x {
            // 分裂到order阶
            while current_order > order as usize {
                current_order -= 1;
                // 把后面那半块放回空闲链表

                let buddy = x + (1 << current_order);
                // debug!("x={:?}, buddy={:?}", x, buddy);
                // debug!("current_order={:?}, buddy={:?}", current_order, buddy);
                unsafe { self.insert_without_merge(buddy, current_order as u8) };
            }
            return Some(x);
        }

        return None;
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
        assert!(count.data().is_power_of_two());
        // 计算需要分配的阶数
        let mut order = log2(count.data());
        if count.data() & ((1 << order) - 1) != 0 {
            order += 1;
        }
        let order = (order + MIN_ORDER) as u8;
        if order as usize >= MAX_ORDER {
            return None;
        }

        // debug!("buddy_alloc: order = {}", order);
        // 获取该阶数的一个空闲页面
        // Preserve scarce DMA32 memory while normal memory is available.
        let normal = if self.has_normal_memory {
            self.pop_front(BuddyZone::Normal, order)
        } else {
            None
        };
        let dma32_fallback_allowed = !self.has_normal_memory
            || self.dma32_free_pages
                >= count
                    .data()
                    .saturating_add(DMA32_FALLBACK_RESERVE_PAGES)
                    .saturating_add(DMA32_ALLOCATION_METADATA_HEADROOM);
        let free_addr = normal.or_else(|| {
            dma32_fallback_allowed
                .then(|| self.pop_front(BuddyZone::Dma32, order))
                .flatten()
        });
        // debug!(
        //     "buddy_alloc: order = {}, free_addr = {:?}",
        //     order,
        //     free_addr
        // );
        return free_addr
            .map(|addr| (addr, PageFrameCount::new(1 << (order as usize - MIN_ORDER))));
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
        assert!(count.data().is_power_of_two());
        let requested_bytes = count.data().checked_mul(A::PAGE_SIZE)?;
        let requested_order = (log2(count.data()) + MIN_ORDER) as u8;
        if requested_order as usize >= MAX_ORDER {
            return None;
        }

        if max_phys_addr.data() == usize::MAX {
            return self.buddy_alloc(count);
        }
        if max_phys_addr.data() == DMA32_LIMIT {
            return self
                .pop_front(BuddyZone::Dma32, requested_order)
                .map(|base| (base, count));
        }

        let zones: &[BuddyZone] = if max_phys_addr.data() < DMA32_LIMIT {
            &[BuddyZone::Dma32]
        } else {
            &[BuddyZone::Normal, BuddyZone::Dma32]
        };
        for &zone in zones {
            for current_order in requested_order as usize..MAX_ORDER {
                let mut list_addr = self.free_area(zone, current_order as u8);
                if list_addr.is_null() {
                    continue;
                }
                loop {
                    let list: PageList<A> = Self::read_page(list_addr);
                    for index in 0..list.entry_num {
                        let entry_addr = Self::entry_virt_addr(list_addr, index);
                        let entry: PhysAddr = unsafe { A::read(entry_addr) };
                        let fits = entry
                            .data()
                            .checked_add(requested_bytes - 1)
                            .is_some_and(|end| end <= max_phys_addr.data());
                        if !fits {
                            continue;
                        }

                        let base = self.remove_entry(zone, current_order as u8, list_addr, index);
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
                        return Some((base, count));
                    }
                    if list.next_page.is_null() {
                        break;
                    }
                    list_addr = list.next_page;
                }
            }
        }
        None
    }

    /// 释放一个块
    ///
    /// ## 参数
    ///
    /// - `base` - 块的起始地址
    /// - `order` - 块的阶数
    unsafe fn buddy_free(&mut self, mut base: PhysAddr, order: u8) {
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

            let mut page_list_paddr = self.free_area(zone, order as u8);
            let mut buddy_entry_index = None;
            let mut buddy_entry_page_list_paddr = None;
            // 除非order是最大的，否则尝试查找伙伴块
            if likely(order != MAX_ORDER - 1) && !page_list_paddr.is_null() {
                'outer: loop {
                    let page_list: PageList<A> = Self::read_page(page_list_paddr);
                    for i in 0..page_list.entry_num {
                        let entry_virt_addr = Self::entry_virt_addr(page_list_paddr, i);
                        let entry: PhysAddr = unsafe { A::read(entry_virt_addr) };
                        if entry == buddy_addr {
                            // 找到了伙伴块，记录该entry相关信息，然后退出查找
                            buddy_entry_index = Some(i);
                            buddy_entry_page_list_paddr = Some(page_list_paddr);
                            break 'outer;
                        }
                    }
                    if page_list.next_page.is_null() {
                        break;
                    }
                    page_list_paddr = page_list.next_page;
                }
            }

            if let Some(buddy_entry_index) = buddy_entry_index {
                let removed = self.remove_entry(
                    zone,
                    order as u8,
                    buddy_entry_page_list_paddr.unwrap(),
                    buddy_entry_index,
                );
                debug_assert_eq!(removed, buddy_addr);
            } else {
                self.insert_without_merge(base, order as u8);
                return;
            }
            base = min(base, buddy_addr);
            order += 1;
        }
        // 走到这一步，order应该为MAX_ORDER-1
        assert!(order == MAX_ORDER - 1);
    }
}

/// Exercise bounded allocation, splitting, and merging on a private arena.
///
/// The live allocator only supplies one aligned backing block.  All operations
/// under test use a separate `BuddyAllocator`, so their exact addresses are
/// deterministic and concurrent kernel allocations cannot affect the result.
#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
pub(crate) fn deterministic_buddy_selftest() -> (bool, bool, bool, bool, bool) {
    const BACKING_PAGES: usize = 128;
    const ARENA_OFFSET_PAGES: usize = 64;
    const ARENA_PAGES: usize = 16;

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
            dma32_empty_area,
            normal_empty_area,
            dma32_free_pages: 0,
            normal_free_pages: 0,
            has_normal_memory: false,
            total: PageFrameCount::new(ARENA_PAGES),
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
            let fragmented_rejects_pair = allocator
                .buddy_alloc_below(PageFrameCount::new(2), arena_max)
                .is_none();

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
            fragmented_rejects_pair
                && exact_pair
                && merged.is_some_and(|(base, count)| base == arena && count.data() == 16)
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
            && dma_still_available.is_some_and(|(base, _)| base == reserve_low);

        selection_ok && reserve_ok
    };

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
                PageList::new(BuddyAllocator::<MMArch>::BUDDY_ENTRIES, next),
            );
            for entry_index in 0..BuddyAllocator::<MMArch>::BUDDY_ENTRIES {
                let ordinal = page_index * BuddyAllocator::<MMArch>::BUDDY_ENTRIES + entry_index;
                unsafe {
                    MMArch::write(
                        BuddyAllocator::<MMArch>::entry_virt_addr(page, entry_index),
                        PhysAddr::new(0x1000_0000 + ordinal * 2 * MMArch::PAGE_SIZE),
                    );
                }
            }
        }
        allocator.dma32_free_area[0] = list_pages[0];
        allocator.dma32_free_pages = list_pages.len() * BuddyAllocator::<MMArch>::BUDDY_ENTRIES;

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
    };

    unsafe {
        deallocate_page_frames(PhysPageFrame::new(backing), backing_count);
    }
    (
        bounded_selection_ok,
        split_merge_ok,
        fragmented_ok,
        dma32_zone_ok,
        metadata_reuse_ok,
    )
}

impl<A: MemoryManagementArch> FrameAllocator for BuddyAllocator<A> {
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
