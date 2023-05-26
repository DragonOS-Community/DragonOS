use crate::{kdebug, kerror, mm};
/// @Auther: Kong
/// @Date: 2023-03-28 16:03:47
/// @FilePath: /DragonOS/kernel/src/mm/allocator/buddy.rs
/// @Description:
use alloc::collections::LinkedList;

use crate::mm::allocator::bump::BumpAllocator;
use crate::mm::allocator::page_frame::{FrameAllocator, PageFrameCount, PageFrameUsage};
use crate::mm::{MemoryManagementArch, PhysAddr, VirtAddr};
use core::cmp::{self, max};
use core::intrinsics::{likely, unlikely};
use core::ops::Add;
use core::{marker::PhantomData, mem};

// 一个全局变量MAX_ORDER，用来表示buddy算法的最大阶数 [MIN_ORDER, MAX_ORDER)左闭右开区间
const MAX_ORDER: usize = 31;
// 4KB
const MIN_ORDER: usize = 12;

/// @brief: 用来表示 buddy 算法中的一个 buddy 块，整体存放在area的头部
// 这种方式会出现对齐问题
// #[repr(packed)]
#[repr(C)]
pub struct BuddyEntry<A> {
    // 代表的页的起始地址
    base: PhysAddr,
    // entry的阶数
    order: u8,
    // entry是否在Buddy中使用
    pg_buddy: bool,
    phantom: PhantomData<A>,
}

impl<A> Clone for BuddyEntry<A> {
    fn clone(&self) -> Self {
        Self {
            base: self.base,
            order: self.order,
            pg_buddy: self.pg_buddy,
            phantom: PhantomData,
        }
    }
}
impl<A> Copy for BuddyEntry<A> {}

impl<A: MemoryManagementArch> BuddyEntry<A> {
    fn empty() -> Self {
        Self {
            base: PhysAddr::new(0),
            order: 0,
            pg_buddy: false,
            phantom: PhantomData,
        }
    }
}

pub struct BuddyAllocator<A> {
    // buddy表的虚拟地址
    table_virt: VirtAddr,
    // 使用free_area来表示 MAX_ORDER 个阶数的空闲数组，每个数组元素都是一个链表，链表的每个元素都是一个BuddyEntry
    free_area: [LinkedList<BuddyEntry<A>>; MAX_ORDER as usize],
    total_used_pages: usize,
    phantom: PhantomData<A>,
}

impl<A: MemoryManagementArch> BuddyAllocator<A> {
    const BUDDY_ENTRIES: usize = A::PAGE_SIZE / mem::size_of::<BuddyEntry<A>>();
    // 定义一个变量记录buddy表的大小
    pub unsafe fn new(mut bump_allocator: BumpAllocator<A>) -> Option<Self> {
        // 获取bump_allocator.areas()的所有area的大小之和，并判断有多少个页
        let mut total_size = 0;
        for area in bump_allocator.areas().iter() {
            total_size += area.size;
        }
        // 计算需要多少个页来存储 buddy 算法的数据结构
        let total_used_pages = (total_size >> A::PAGE_SHIFT) / Self::BUDDY_ENTRIES;
        // 申请buddy_pages个页，用于存储 buddy 算法的数据结构
        let table_phys = bump_allocator.allocate_one()?;
        for _ in 0..total_used_pages - 1 {
            bump_allocator.allocate_one()?;
        }
        let table_virt = A::phys_2_virt(table_phys);
        let table_virt = table_virt?;
        // 将申请到的内存全部分配为 BuddyEntry<A> 类型
        for i in 0..Self::BUDDY_ENTRIES * total_used_pages {
            let virt = table_virt.add(i * mem::size_of::<BuddyEntry<A>>());
            A::write(virt, BuddyEntry::<A>::empty());
        }
        // 初始化free_area

        let free_area = Default::default();

        let mut allocator = Self {
            table_virt,
            free_area,
            total_used_pages,
            phantom: PhantomData,
        };
        for old_area in bump_allocator.areas().iter() {
            let mut area = old_area.clone();
            // 如果offset大于area的起始地址，那么需要跳过offset的大小
            if bump_allocator.offset() > area.base.data() {
                area.base = area.base.add(bump_allocator.offset());
                area.size -= bump_allocator.offset();
            }
            // 将area的起始地址对齐到最大的阶数
            let new_offset = (area.base.data() + (1 << MAX_ORDER) - 1) & !((1 << MAX_ORDER) - 1);
            area.size -= new_offset - area.base.data();
            area.base = area.base.add(new_offset);

            // 如果area的大小大于2^MAX_ORDER，那么将area分割为多个area
            while area.size > (1 << MAX_ORDER) {
                let mut new_area = area.clone();
                new_area.size = 1 << MAX_ORDER;
                area.base = area.base.add(1 << MAX_ORDER);
                area.size -= 1 << MAX_ORDER;
                allocator.add_area(new_area);
            }
            // TODO 对于分配的内存的前后两段空间，需不需要被分配出去？5
        }

        Some(allocator)
    }

    /// @brief: 将一个area添加到free_area中
    /// @param {type}
    /// @area: 要添加的area
    unsafe fn add_area(&mut self, area: mm::PhysMemoryArea) {
        // 计算area的阶数
        let order = (area.size >> A::PAGE_SHIFT) as u8;
        // 计算area的起始地址
        let base = area.base;
        let pg_buddy = false;
        let entry = BuddyEntry {
            base,
            order,
            pg_buddy,
            phantom: PhantomData,
        };
        self.add_entry(entry);
    }

    /// @brief: 移除一个entry
    /// @param  entry
    pub fn remove_entry(&mut self, entry: BuddyEntry<A>) {
        let order = entry.order as usize;
        let mut count = 0;
        // 在迭代free_area时使用count统计次数
        for i in self.free_area[order].iter_mut() {
            // 如果i的起始地址等于entry的伙伴的起始地址，那么就将i从free_area中移除
            if i.base.data() == entry.base.data() {
                break;
            }
            count += 1;
        }
        let mut split_list = self.free_area[order].split_off(count);
        split_list.pop_front();
        self.free_area[order].append(&mut split_list);
    }
    /// @brief: 将entry添加到free_area和写入内存中
    /// @param  mut
    /// @param  entry
    unsafe fn add_entry(&mut self, entry: BuddyEntry<A>) {
        let order = entry.order as usize;
        if entry.pg_buddy == false {
            self.free_area[order].push_back(entry);
        }
        let virt = self.table_virt.add(entry.base.data() >> A::PAGE_SHIFT);
        A::write(virt, entry);
    }
    /// @brief: 从内存中读入entry
    /// @param  offset
    /// @return BuddyEntry<A>
    unsafe fn read_entry(&self, offset: usize) -> BuddyEntry<A> {
        let virt = self.table_virt.add(offset);
        return A::read(virt);
    }
}

impl<A: MemoryManagementArch> FrameAllocator for BuddyAllocator<A> {
    unsafe fn allocate(&mut self, count: PageFrameCount) -> Option<PhysAddr> {
        // 如果table_virt为0，说明buddy还没有初始化，那么就直接返回None
        if self.table_virt.data() == 0 {
            return None;
        }

        // 计算要分配的页的阶数
        let mut order = 0 as u8;
        while (1 << order) < count.data() {
            order += 1;
        }

        // 从free_area中找到第一个能够满足要求的area
        let mut entry = self.free_area[order as usize].pop_front();
        while entry.is_none() {
            order += 1;
            if order >= MAX_ORDER as u8{
                return None;
            }
            entry = self.free_area[order as usize].pop_front();
        }
        let mut entry = entry.unwrap();

        // 如果entry的阶数大于要分配的阶数，那么将entry分割
        while entry.order > order {
            entry.order -= 1;
            let new_entry = BuddyEntry {
                base: entry.base.add(1 << entry.order),
                order: entry.order,
                pg_buddy: false,
                phantom: PhantomData,
            };
            // // 将拆分后的entry的伙伴写入table_virt，并将伙伴添加到free_area中
            self.add_entry(new_entry)
        }

        // 更新entry的使用情况
        entry.pg_buddy = true;
        entry.order = order;
        self.add_entry(entry);
        Some(entry.base)
    }

    unsafe fn free(&mut self, base: PhysAddr, count: PageFrameCount) {
        // 计算base对应的entry的虚拟地址
        let start_page = base.data() >> A::PAGE_SHIFT;
        let mut entry = self.read_entry(start_page * mem::size_of::<BuddyEntry<A>>());

        // 如果entry的pg_buddy为false，说明entry已经被释放了，那么就直接返回
        if entry.pg_buddy == false {
            return;
        }
        // 将entry的pg_buddy设置为false
        entry.pg_buddy = false;
        self.add_entry(entry);

        // 如果entry的阶数小于MAX_ORDER，那么就将entry合并到buddy中
        while entry.order < MAX_ORDER as u8 {
            // 获取entry的伙伴的页号
            let buddy_page = if entry.base.data() & (1 << entry.order) == 0 {
                start_page + 1
            } else {
                start_page - 1
            };
            let buddy = self.read_entry(buddy_page * mem::size_of::<BuddyEntry<A>>());
            // 如果entry的buddy的阶数不等于entry的阶数，或者entry的buddy的pg_buddy位为1，那么就退出循环
            if buddy.order != entry.order || buddy.pg_buddy == true {
                break;
            }
            self.add_entry(buddy);

            // 遍历free_area[entry.order]中的链表，移除buddy
            self.remove_entry(buddy);
            // 将entry的buddy从free_area中移除，因为entry和entry的buddy已经合并了
            self.remove_entry(entry);
            // 将entry的阶数加1
            entry.order += 1;
            // 如果entry的起始地址大于entry的buddy的起始地址，那么就将entry的起始地址设置为entry的buddy的起始地址
            if entry.base.data() > buddy.base.data() {
                entry.base = buddy
                    .base
                    .add((1 << (entry.order + (A::PAGE_SHIFT as u8))) - (1 << A::PAGE_SHIFT));
            }

            // 将entry添加到free_area中
            self.add_entry(entry);
        }
    }
    unsafe fn usage(&self) -> PageFrameUsage {
        let mut total = 0;
        let mut used = 0;
        // 遍历所有的buddy，计算已经使用的页和总共的页
        let mut i = 0;
        while i < self.total_used_pages * Self::BUDDY_ENTRIES {
            let entry = self.read_entry(i * mem::size_of::<BuddyEntry<A>>());
            total += 1 << entry.order;
            if entry.pg_buddy == true {
                used += 1 << entry.order;
                // 让i跳过已经使用的页
                i += 1 << entry.order;
                break;
            }
        }
        let frame = PageFrameUsage::new(PageFrameCount::new(used), PageFrameCount::new(total));
        return frame;
    }
}

// ====== 计算 Buddy预留内存页的代码 BEGIN =====

// Buddy预留内存页的计算结果
static mut PRESERVE_PAGES_RESULT: [BuddyPreservePageResult; MAX_ORDER - MIN_ORDER] =
    [BuddyPreservePageResult::zeroed(); MAX_ORDER - MIN_ORDER];

#[derive(Debug, Clone)]
enum CalculateError {
    PagesError,
    EntriesError,
    NoEnoughPages,
}

struct PreservePageCalculator {
    layers: [BuddyCalculatorLayer; MAX_ORDER - MIN_ORDER],
    /// 总的页数
    total_pages: usize,
    /// 每个页能够存放的buddy entry的数量
    entries_per_page: usize,
    max_order: usize,
}

macro_rules! calculator_layer {
    ($self: ident, $order: expr) => {
        $self.layers[$order - MIN_ORDER]
    };
}

impl PreservePageCalculator {
    const PAGE_4K: usize = (1 << 12);
    const PAGE_1G: usize = (1 << 30);
    const MAX_ORDER_SIZE: usize = (1 << (MAX_ORDER - 1));

    const fn new(entries_per_page: usize) -> Self {
        PreservePageCalculator {
            layers: [BuddyCalculatorLayer::new(); MAX_ORDER - MIN_ORDER],
            total_pages: 0,
            entries_per_page,
            max_order: 0,
        }
    }

    /// ## 开始仿真计算
    ///
    /// ## 参数
    ///
    /// * `pages` - 交给buddy管理的总的页数
    ///
    /// ## 返回
    ///
    /// * `&'static [BuddyCalculatorResult]` - 计算结果，每个元素表示一个阶数的buddy的计算结果。包含这个阶数需要的页数和链表内的buddy entry的数量
    fn calculate(
        &mut self,
        pages: usize,
    ) -> Result<&'static [BuddyPreservePageResult], CalculateError> {
        self.total_pages = pages;
        self.init_layers();

        self.sim()?;

        // 将结果保存到PRESERVE_PAGES_RESULT中
        for order in MIN_ORDER..MAX_ORDER {
            let layer = &calculator_layer!(self, order);

            unsafe {
                PRESERVE_PAGES_RESULT[order - MIN_ORDER] =
                    BuddyPreservePageResult::new(order, layer.allocated_pages, layer.entries);
            }
        }
        // 检查结果是否合法
        self.check_result(unsafe { &PRESERVE_PAGES_RESULT })?;
        return Ok(unsafe { &PRESERVE_PAGES_RESULT });
    }

    fn sim(&mut self) -> Result<(), CalculateError> {
        loop {
            let mut flag = false;
            'outer: for order in (MIN_ORDER..MAX_ORDER).rev() {
                let mut to_alloc =
                    self.pages_need_to_alloc(order, calculator_layer!(self, order).entries);
                // 模拟申请
                while to_alloc > 0 {
                    let page4k = calculator_layer!(self, MIN_ORDER).entries;
                    let page4k = cmp::min(page4k, to_alloc);
                    calculator_layer!(self, order).allocated_pages += page4k;
                    calculator_layer!(self, MIN_ORDER).entries -= page4k;
                    to_alloc -= page4k;

                    if to_alloc == 0 {
                        break;
                    }

                    // 从最小的order开始找，然后分裂
                    let split_order = ((MIN_ORDER + 1)..=order).find(|&i| {
                        let layer = &calculator_layer!(self, i);
                        // println!("find: order: {}, entries: {}", i, layer.entries);
                        layer.entries > 0
                    });

                    if let Some(split_order) = split_order {
                        for i in (MIN_ORDER + 1..=split_order).rev() {
                            let layer = &mut calculator_layer!(self, i);
                            layer.entries -= 1;
                            calculator_layer!(self, i - 1).entries += 2;
                        }
                    } else {
                        // 从大的开始分裂
                        let split_order = ((order + 1)..MAX_ORDER).find(|&i| {
                            let layer = &calculator_layer!(self, i);
                            // println!("find: order: {}, entries: {}", i, layer.entries);
                            layer.entries > 0
                        });
                        if let Some(split_order) = split_order {
                            for i in (order + 1..=split_order).rev() {
                                let layer = &mut calculator_layer!(self, i);
                                layer.entries -= 1;
                                calculator_layer!(self, i - 1).entries += 2;
                            }
                            flag = true;
                            break 'outer;
                        } else {
                            if order == MIN_ORDER
                                && to_alloc == 1
                                && calculator_layer!(self, MIN_ORDER).entries > 0
                            {
                                calculator_layer!(self, MIN_ORDER).entries -= 1;
                                calculator_layer!(self, MIN_ORDER).allocated_pages += 1;
                                break;
                            } else {
                                kerror!("BuddyPageCalculator::sim: NoEnoughPages: order: {}, pages_needed: {}",  order, to_alloc);
                                return Err(CalculateError::NoEnoughPages);
                            }
                        }
                    }
                }
            }

            if !flag {
                break;
            }
        }
        return Ok(());
    }

    fn init_layers(&mut self) {
        let max_order = cmp::min(log2(self.total_pages * Self::PAGE_4K), MAX_ORDER - 1);

        self.max_order = max_order;
        let mut remain_bytes = self.total_pages * Self::PAGE_4K;
        for order in (MIN_ORDER..=max_order).rev() {
            let entries = remain_bytes / (1 << order);
            remain_bytes -= entries * (1 << order);
            calculator_layer!(self, order).entries = entries;
            // kdebug!(
            //     "order: {}, entries: {}, pages: {}",
            //     order,
            //     entries,
            //     calculator_layer!(self, order).allocated_pages
            // );
        }
    }

    fn entries_to_page(&self, entries: usize) -> usize {
        (entries + self.entries_per_page - 1) / self.entries_per_page
    }

    fn pages_needed(&self, entries: usize) -> usize {
        max(1, self.entries_to_page(entries))
    }
    fn pages_need_to_alloc(&self, order: usize, current_entries: usize) -> usize {
        let allocated = calculator_layer!(self, order).allocated_pages;
        let tot_need = self.pages_needed(current_entries);
        if tot_need > allocated {
            tot_need - allocated
        } else {
            0
        }
    }
    fn check_result(
        &self,
        results: &'static [BuddyPreservePageResult],
    ) -> Result<(), CalculateError> {
        // 检查pages是否正确
        let mut total_pages = 0;
        for r in results.iter() {
            total_pages += r.pages;
            total_pages += r.entries * (1 << r.order) / Self::PAGE_4K;
        }
        if unlikely(total_pages != self.total_pages) {
            // println!("total_pages: {}, self.total_pages: {}", total_pages, self.total_pages);
            kerror!(
                "total_pages: {}, self.total_pages: {}",
                total_pages,
                self.total_pages
            );
            return Err(CalculateError::PagesError);
        }
        // 在确认pages正确的情况下，检查每个链表的entries是否正确
        // 检查entries是否正确
        for r in results.iter() {
            let pages_needed = self.pages_needed(r.entries);
            if pages_needed != r.pages {
                if likely(
                    r.order == (MAX_ORDER - 1)
                        && (pages_needed as isize - r.pages as isize).abs() == 1,
                ) {
                    continue;
                }
                kerror!(
                    "order: {}, pages_needed: {}, pages: {}",
                    r.order,
                    self.pages_needed(r.entries),
                    r.pages
                );
                return Err(CalculateError::EntriesError);
            }
        }
        return Ok(());
    }
}

#[derive(Debug, Clone, Copy)]
struct BuddyCalculatorLayer {
    /// 当前层的buddy entry的数量
    entries: usize,
    allocated_pages: usize,
}

impl BuddyCalculatorLayer {
    const fn new() -> Self {
        BuddyCalculatorLayer {
            entries: 0,
            allocated_pages: 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BuddyPreservePageResult {
    order: usize,
    pages: usize,
    entries: usize,
}

impl BuddyPreservePageResult {
    fn new(order: usize, pages: usize, entries: usize) -> Self {
        BuddyPreservePageResult {
            order,
            pages,
            entries,
        }
    }

    const fn zeroed() -> Self {
        BuddyPreservePageResult {
            order: 0,
            pages: 0,
            entries: 0,
        }
    }
}

/// 一个用于计算整数的对数的函数，会向下取整。（由于内核不能进行浮点运算，因此需要这个函数）
fn log2(x: usize) -> usize {
    let leading_zeros = x.leading_zeros() as usize;
    let log2x = 63 - leading_zeros;
    return log2x;
}
// ====== 计算 Buddy预留内存页的代码 END =====
