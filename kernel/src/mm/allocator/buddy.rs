/// @Auther: Kong
/// @Date: 2023-03-28 16:03:47
/// @FilePath: /DragonOS/kernel/src/mm/allocator/buddy.rs
/// @Description:
use crate::mm::allocator::bump::BumpAllocator;
use crate::mm::allocator::page_frame::{FrameAllocator, PageFrameCount, PageFrameUsage};
use crate::mm::{MemoryManagementArch, PhysAddr, VirtAddr};
use crate::{kdebug, kerror};
use core::cmp::{self, max};
use core::intrinsics::{likely, unlikely};
use core::{marker::PhantomData, mem};

// 一个全局变量MAX_ORDER，用来表示buddy算法的最大阶数 [MIN_ORDER, MAX_ORDER)左闭右开区间
const MAX_ORDER: usize = 31;
// 4KB
const MIN_ORDER: usize = 12;

// 保存buddy算法中每一页存放的BuddyEntry的信息，占据每个页的起始位置
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

pub struct BuddyAllocator<A> {
    // 存放每个阶的空闲“链表”的头部地址
    free_area: [PhysAddr; (MAX_ORDER - MIN_ORDER + 1) as usize],
    phantom: PhantomData<A>,
}
impl<A: MemoryManagementArch> BuddyAllocator<A> {
    const BUDDY_ENTRIES: usize =
        (A::PAGE_SIZE - mem::size_of::<PageList<A>>()) / mem::size_of::<PhysAddr>();
    // 定义一个变量记录buddy表的大小
    pub unsafe fn new(mut bump_allocator: BumpAllocator<A>) -> Option<Self> {
        let areas = bump_allocator.areas();
        // 计算总的表的大小，目前初始化只管理第一个area的大小
        let total_table_size = areas[0].size;
        let base_addr = areas[0].base.data();
        let mut offset = 0;
        // 初始化一个PreservePageCalculator，用来计算buddy表的大小
        let mut calculator = PreservePageCalculator::new(Self::BUDDY_ENTRIES);
        let test = calculator.calculate(total_table_size);
        // 获取test中的BuddyPreservePageResult
        let result = test.unwrap();
        let mut free_area: [PhysAddr; (MAX_ORDER - MIN_ORDER + 1) as usize] =
            [PhysAddr::new(0); (MAX_ORDER - MIN_ORDER + 1) as usize];

        // 根据Buddy占用的空间，默认分配area最前面的空间，计算分配后的 offset
        for i in 0..result.len(){
            offset += result[i].pages <<A::PAGE_SHIFT;
        }
        // 打印offset
        kdebug!("offset {:b}", offset);

        // 初始化每个阶的空闲链表
        for i in 0..result.len() {
            let order = result[i].order;
            let need_pages = result[i].pages;
            let mut total_entries = result[i].entries;
            let mut curr_page = bump_allocator.allocate_one();
            // 保存每个阶的空闲链表的头部地址
            free_area[i] = curr_page.unwrap();

            let mut page_list: PageList<A>;
            let mut next_page: Option<PhysAddr>;
            // 依次初始化每个阶的page_list
            for _ in 0..need_pages {
                if total_entries > Self::BUDDY_ENTRIES {
                    total_entries -= Self::BUDDY_ENTRIES;
                    next_page = bump_allocator.allocate_one();
                    page_list = PageList::<A>::new(Self::BUDDY_ENTRIES, next_page.unwrap());
                    // 写入pagelist到curr_page
                    Self::write_page(curr_page.unwrap(), page_list);

                    // 计算要存放的entry的地址
                    let mut entry_addr = curr_page.unwrap().add(mem::size_of::<PageList<A>>());
                    // 写入entry
                    for _ in 0..Self::BUDDY_ENTRIES {
                        let entry_virt_addr = A::phys_2_virt(entry_addr);
                        A::write(entry_virt_addr?, PhysAddr::new(base_addr + offset));
                        // 计算2的order次幂，加到offset上
                        offset += 1 << (order+A::PAGE_SHIFT);

                        entry_addr = entry_addr.add(mem::size_of::<PhysAddr>());
                    }

                    curr_page = next_page;
                } else {
                    // 不需要新的页面
                    page_list = PageList::<A>::new(total_entries, PhysAddr(0));
                    let virt_addr = A::phys_2_virt(curr_page.unwrap());
                    A::write(virt_addr?, page_list);
                    // 计算要存放的entry的地址
                    let mut entry_addr = curr_page.unwrap().add(mem::size_of::<PageList<A>>());
                    // 写入entry
                    for _ in 0..total_entries {
                        let entry_virt_addr = A::phys_2_virt(entry_addr);
                        A::write(entry_virt_addr?, PhysAddr::new(base_addr + offset));
                        // 计算2的order次幂，加到offset上
                        offset += 1 << (order+A::PAGE_SHIFT);
                        entry_addr = entry_addr.add(mem::size_of::<PhysAddr>());
                    }
                }
            }
        }
        // 打印free_area的地址
        for i in 0..free_area.len() - 1 {
            // kdebug!("free_area[{}]: {:b}", i, free_area[i].data());
            let virt_addr = A::phys_2_virt(free_area[i]);
            let mut page_list: PageList<A> = A::read(virt_addr?);

            while page_list.next_page.data() != 0 {
                let next_page_phy_addr = page_list.next_page;
                let virt_addr = A::phys_2_virt(next_page_phy_addr);
                page_list = A::read(virt_addr?);

            }
        }
        let allocator = Self {
            free_area: free_area,
            phantom: PhantomData,
        };

        Some(allocator)
    }
    // 获取第j个entry的虚拟地址
    pub fn entry_virt_addr( base_addr: usize, j: usize) -> VirtAddr {
        let entry_virt_addr = unsafe {
            A::phys_2_virt(Self::entry_addr(base_addr, j))
        };
        return entry_virt_addr.unwrap();
    }
    pub fn entry_addr(base_addr: usize, j: usize) -> PhysAddr {
        let entry_addr = PhysAddr::new(
            base_addr + mem::size_of::<PageList<A>>() + j * mem::size_of::<PhysAddr>(),
        );
        return entry_addr;
    }
    pub fn read_page<T>(addr:PhysAddr)->T{
        let page_list = unsafe { A::read(A::phys_2_virt(addr).unwrap()) };
        return page_list;
    }

    pub fn write_page(curr_page: PhysAddr, page_list: PageList<A>) {
        // 把物理地址转换为虚拟地址
        let virt_addr = unsafe { A::phys_2_virt(curr_page) };
        let virt_addr = virt_addr.unwrap();
        unsafe { A::write(virt_addr, page_list) };
    }
    // 从order+1开始，向oder分裂
    pub fn split(&mut self, order: u8) {
        // 从order+1开始，向oder分裂
        if order + 1 > (MAX_ORDER - MIN_ORDER - 1) as u8 {
            panic!("order is out of range");
        }
        // 判断order+1是否有空闲页面
        let next_order = order + 1;
        let next_page_list_addr = self.free_area[next_order as usize];
        let mut next_page_list: PageList<A> =Self::read_page(next_page_list_addr);
        // 若page_list的entry_num为0，说明没有空闲页面,需要上一层分裂
        if next_page_list.entry_num == 0 {
            self.split(next_order);
            next_page_list = Self::read_page(next_page_list_addr);
        }
        // 找到最后一个页面地址
        while next_page_list.next_page.data() != 0 {
            next_page_list = Self::read_page(next_page_list.next_page);
        }
        // 找到页的最后一个entry的地址
        // 取出最后一个entry的值
        let father_entry_virt_addr=Self::entry_virt_addr(next_page_list_addr.data(),next_page_list.entry_num-1);
        let father_entry: PhysAddr = unsafe { A::read(father_entry_virt_addr) };
        // entry_num减一
        next_page_list.entry_num -= 1;
        // 写回
        unsafe { A::write(A::phys_2_virt(next_page_list_addr).unwrap(), next_page_list) };

        // 获取order的空闲链表的地址
        let page_list_addr = self.free_area[order as usize];
        // 将entry分成两个，获取其物理地址
        let entry1 = father_entry;
        let entry2 = father_entry.add((1<<(order+A::PAGE_SHIFT as u8)) as usize);
        // 获取两个子entry需要写入的地址，找到order的空闲链表的最后一个entry的地址
        let mut page_list: PageList<A> =Self::read_page(page_list_addr);
        let entry1_virt_addr=Self::entry_virt_addr(page_list_addr.data(),page_list.entry_num);
        let entry2_virt_addr=Self::entry_virt_addr(page_list_addr.data(),page_list.entry_num+1);
        // 将entry1写回
        unsafe { A::write(entry1_virt_addr, entry1) };
        // 将entry2写回
        unsafe { A::write(entry2_virt_addr, entry2) };

        // order的空闲链表的entry_num加2
        page_list.entry_num += 2;
        unsafe { A::write(A::phys_2_virt(page_list_addr).unwrap(), page_list) };
    }
    // 在order阶的“空闲链表”末尾分配一个页面
    pub fn pop_tail(&mut self, order: u8) -> Option<PhysAddr> {
        let mut page_list_addr = self.free_area[order as usize];
        let mut page_list: PageList<A> = Self::read_page(page_list_addr);
        let mut prev_page_list_addr = PhysAddr(0);
        // 若page_list的entry_num为0，说明没有空闲页面,需要上一层分裂
        if page_list.entry_num == 0 {
            self.split(order);
            page_list = Self::read_page(page_list_addr);
        }

        // 找到最后一个页面地址
        while page_list.next_page.data() != 0 {
            prev_page_list_addr = page_list_addr;
            let virt_addr = unsafe { A::phys_2_virt(page_list.next_page) };
            page_list = unsafe { A::read(virt_addr.unwrap()) };
            page_list_addr = page_list.next_page;
        }
        // 在page_list中获取最后一个entry的地址
        // TODO :确定这里会不会出现entry_num为0的情况
        // 读取最后一个entry的内容
        let last_entry: PhysAddr=unsafe { A::read(Self::entry_virt_addr(page_list_addr.data(), page_list.entry_num - 1)) };
        // 更新page_list的entry_num
        page_list.entry_num -= 1;
        if page_list.entry_num == 0 {
            if prev_page_list_addr.data() != 0 {
                // 此时page_list已经没有空闲页面了，又因为非唯一页，需要删除该page_list
                // 把prev_page_list的next_page置为0
                let mut prev_page_list: PageList<A> = Self::read_page(prev_page_list_addr);
                prev_page_list.next_page = PhysAddr(0);
                // 把更新后的prev_page_list写回
                Self::write_page(prev_page_list_addr, prev_page_list);
            } else {
                // 唯一页，不能删除
                // 把更新后的page_list写回
                Self::write_page(page_list_addr, page_list);
            }
        } else {
            // 若entry_num不为0，说明该page_list还有空闲页面，需要更新该page_list
            // 把更新后的page_list写回
            Self::write_page(page_list_addr, page_list);
        }
        Some(last_entry)
    }
}

impl<A: MemoryManagementArch> FrameAllocator for BuddyAllocator<A> {
    unsafe fn allocate(&mut self, count: PageFrameCount) -> Option<PhysAddr> {
        // 计算需要分配的阶数
        let mut order = 0 as u8;
        while (1 << order) < count.data() {
            order += 1;
        }
        // 如果阶数超过最大阶数，返回None
        if order > (MAX_ORDER - MIN_ORDER - 1) as u8 {
            panic!("order {} is not supported", order);
        }
        // 获取该阶数的一个空闲页面
        let free_addr = self.pop_tail(order);
        return free_addr;
    }

    unsafe fn free(&mut self, base: PhysAddr, count: PageFrameCount) {}
    unsafe fn usage(&self) -> PageFrameUsage {
        let frame = PageFrameUsage::new(PageFrameCount::new(0), PageFrameCount::new(0));
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
