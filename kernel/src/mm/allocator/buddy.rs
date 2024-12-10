use log::{debug, warn};

/// @Author: longjin@dragonos.org
/// @Author: kongweichao@dragonos.org
/// @Date: 2023-03-28 16:03:47
/// @FilePath: /DragonOS/kernel/src/mm/allocator/buddy.rs
/// @Description: 伙伴分配器
use crate::arch::MMArch;
use crate::mm::allocator::bump::BumpAllocator;
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
    // 存放每个阶的空闲“链表”的头部地址
    free_area: [PhysAddr; MAX_ORDER - MIN_ORDER],
    /// 总页数
    total: PageFrameCount,
    phantom: PhantomData<A>,
}

impl<A: MemoryManagementArch> BuddyAllocator<A> {
    const BUDDY_ENTRIES: usize =
        // 定义一个变量记录buddy表的大小
        (A::PAGE_SIZE - mem::size_of::<PageList<A>>()) / mem::size_of::<PhysAddr>();

    pub unsafe fn new(mut bump_allocator: BumpAllocator<A>) -> Option<Self> {
        let initial_free_pages = bump_allocator.usage().free();
        let total_memory = bump_allocator.usage().total();
        debug!("Free pages before init buddy: {:?}", initial_free_pages);
        debug!("Buddy entries: {}", Self::BUDDY_ENTRIES);

        let mut free_area: [PhysAddr; MAX_ORDER - MIN_ORDER] =
            [PhysAddr::new(0); MAX_ORDER - MIN_ORDER];

        // Buddy初始占用的空间从bump分配
        for f in free_area.iter_mut() {
            let curr_page = bump_allocator.allocate_one();
            // 保存每个阶的空闲链表的头部地址
            *f = curr_page.unwrap();
            // 清空当前页
            core::ptr::write_bytes(MMArch::phys_2_virt(*f)?.data() as *mut u8, 0, A::PAGE_SIZE);

            let page_list: PageList<A> = PageList::new(0, PhysAddr::new(0));
            Self::write_page(*f, page_list);
        }

        let mut allocator = Self {
            free_area,
            total: PageFrameCount::new(0),
            phantom: PhantomData,
        };

        let mut total_pages_to_buddy = PageFrameCount::new(0);
        let mut res_areas = [PhysMemoryArea::default(); 128];
        let mut offset_in_remain_area = bump_allocator
            .remain_areas(&mut res_areas)
            .expect("BuddyAllocator: failed to get remain areas from bump allocator");

        let remain_areas = &res_areas[0..];

        for area in remain_areas {
            let mut paddr = (area.area_base_aligned() + offset_in_remain_area).data();
            let mut remain_pages =
                PageFrameCount::from_bytes(area.area_end_aligned().data() - paddr).unwrap();

            if remain_pages.data() == 0 {
                continue;
            }
            debug!("area: {area:?}, paddr: {paddr:#x}, remain_pages: {remain_pages:?}");

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

    /// 从空闲链表的开头，取出1个指定阶数的伙伴块，如果没有，则返回None
    ///
    /// ## 参数
    ///
    /// - `order` - 伙伴块的阶数
    fn pop_front(&mut self, order: u8) -> Option<PhysAddr> {
        let mut alloc_in_specific_order = |spec_order: u8| {
            // 先尝试在order阶的“空闲链表”的开头位置分配一个伙伴块
            let mut page_list_addr = self.free_area[Self::order2index(spec_order)];
            let mut page_list: PageList<A> = Self::read_page(page_list_addr);

            // 循环删除头部的空闲链表页
            while page_list.entry_num == 0 {
                let next_page_list_addr = page_list.next_page;
                // 找完了，都是空的
                if next_page_list_addr.is_null() {
                    return None;
                }

                if !next_page_list_addr.is_null() {
                    // 此时page_list已经没有空闲伙伴块了，又因为非唯一页，需要删除该page_list
                    self.free_area[Self::order2index(spec_order)] = next_page_list_addr;
                    // debug!("FREE: page_list_addr={:b}", page_list_addr.data());
                    unsafe {
                        self.buddy_free(page_list_addr, MMArch::PAGE_SHIFT as u8);
                    }
                }
                // 由于buddy_free可能导致首部的链表页发生变化，因此需要重新读取
                let next_page_list_addr = self.free_area[Self::order2index(spec_order)];
                assert!(!next_page_list_addr.is_null());
                page_list = Self::read_page(next_page_list_addr);
                page_list_addr = next_page_list_addr;
            }

            // 有空闲页面，直接分配
            if page_list.entry_num > 0 {
                let entry: PhysAddr = unsafe {
                    A::read(Self::entry_virt_addr(
                        page_list_addr,
                        page_list.entry_num - 1,
                    ))
                };
                // 清除该entry
                unsafe {
                    A::write(
                        Self::entry_virt_addr(page_list_addr, page_list.entry_num - 1),
                        PhysAddr::new(0),
                    )
                };
                if entry.is_null() {
                    panic!(
                        "entry is null, entry={:?}, order={}, entry_num = {}",
                        entry,
                        spec_order,
                        page_list.entry_num - 1
                    );
                }
                // debug!("entry={entry:?}");

                // 更新page_list的entry_num
                page_list.entry_num -= 1;
                let tmp_current_entry_num = page_list.entry_num;
                if page_list.entry_num == 0 {
                    if !page_list.next_page.is_null() {
                        // 此时page_list已经没有空闲伙伴块了，又因为非唯一页，需要删除该page_list
                        self.free_area[Self::order2index(spec_order)] = page_list.next_page;
                        let _ = page_list;
                        unsafe { self.buddy_free(page_list_addr, MMArch::PAGE_SHIFT as u8) };
                    } else {
                        Self::write_page(page_list_addr, page_list);
                    }
                } else {
                    // 若entry_num不为0，说明该page_list还有空闲伙伴块，需要更新该page_list
                    // 把更新后的page_list写回
                    Self::write_page(page_list_addr, page_list.clone());
                }

                // 检测entry 是否对齐
                if !entry.check_aligned(1 << spec_order) {
                    panic!("entry={:?} is not aligned, spec_order={spec_order}, page_list.entry_num={}", entry, tmp_current_entry_num);
                }
                return Some(entry);
            }
            return None;
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
        if x.is_some() {
            // 分裂到order阶
            while current_order > order as usize {
                current_order -= 1;
                // 把后面那半块放回空闲链表

                let buddy = *x.as_ref().unwrap() + (1 << current_order);
                // debug!("x={:?}, buddy={:?}", x, buddy);
                // debug!("current_order={:?}, buddy={:?}", current_order, buddy);
                unsafe { self.buddy_free(buddy, current_order as u8) };
            }
            return x;
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
        let free_addr = self.pop_front(order);
        // debug!(
        //     "buddy_alloc: order = {}, free_addr = {:?}",
        //     order,
        //     free_addr
        // );
        return free_addr
            .map(|addr| (addr, PageFrameCount::new(1 << (order as usize - MIN_ORDER))));
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

            let first_page_list_paddr = self.free_area[Self::order2index(order as u8)];
            let mut page_list_paddr = first_page_list_paddr;
            let mut page_list: PageList<A> = Self::read_page(page_list_paddr);
            let first_page_list = page_list.clone();

            let mut buddy_entry_virt_vaddr = None;
            let mut buddy_entry_page_list_paddr = None;
            // 除非order是最大的，否则尝试查找伙伴块
            if likely(order != MAX_ORDER - 1) {
                'outer: loop {
                    for i in 0..page_list.entry_num {
                        let entry_virt_addr = Self::entry_virt_addr(page_list_paddr, i);
                        let entry: PhysAddr = unsafe { A::read(entry_virt_addr) };
                        if entry == buddy_addr {
                            // 找到了伙伴块，记录该entry相关信息，然后退出查找
                            buddy_entry_virt_vaddr = Some(entry_virt_addr);
                            buddy_entry_page_list_paddr = Some(page_list_paddr);
                            break 'outer;
                        }
                    }
                    if page_list.next_page.is_null() {
                        break;
                    }
                    page_list_paddr = page_list.next_page;
                    page_list = Self::read_page(page_list_paddr);
                }
            }

            // 如果没有找到伙伴块
            if let Some(buddy_entry_virt_addr) = buddy_entry_virt_vaddr {
                // 如果找到了伙伴块，合并，向上递归

                // 伙伴块所在的page_list的物理地址
                let buddy_entry_page_list_paddr = buddy_entry_page_list_paddr.unwrap();

                let mut page_list_paddr = self.free_area[Self::order2index(order as u8)];
                let mut page_list = Self::read_page::<PageList<A>>(page_list_paddr);
                // 找第一个有空闲块的链表页。跳过空闲链表页。不进行回收的原因是担心出现死循环
                while page_list.entry_num == 0 {
                    if page_list.next_page.is_null() {
                        panic!(
                            "buddy_free: page_list.entry_num == 0 && page_list.next_page.is_null()"
                        );
                    }
                    page_list_paddr = page_list.next_page;
                    page_list = Self::read_page(page_list_paddr);
                }

                // 如果伙伴块不在第一个链表页，则把第一个链表中的某个空闲块替换到伙伴块的位置
                if page_list_paddr != buddy_entry_page_list_paddr {
                    let entry: PhysAddr = unsafe {
                        A::read(Self::entry_virt_addr(
                            page_list_paddr,
                            page_list.entry_num - 1,
                        ))
                    };
                    // 把这个空闲块写入到伙伴块的位置
                    unsafe {
                        A::write(buddy_entry_virt_addr, entry);
                    }
                    // 设置刚才那个entry为空
                    unsafe {
                        A::write(
                            Self::entry_virt_addr(page_list_paddr, page_list.entry_num - 1),
                            PhysAddr::new(0),
                        );
                    }
                    // 更新当前链表页的统计数据
                    page_list.entry_num -= 1;
                    Self::write_page(page_list_paddr, page_list);
                } else {
                    // 伙伴块所在的链表页就是第一个链表页
                    let last_entry: PhysAddr = unsafe {
                        A::read(Self::entry_virt_addr(
                            page_list_paddr,
                            page_list.entry_num - 1,
                        ))
                    };

                    // 如果最后一个空闲块不是伙伴块，则把最后一个空闲块移动到伙伴块的位置
                    // 否则后面的操作也将删除这个伙伴块
                    if last_entry != buddy_addr {
                        unsafe {
                            A::write(buddy_entry_virt_addr, last_entry);
                            A::write(
                                Self::entry_virt_addr(page_list_paddr, page_list.entry_num - 1),
                                PhysAddr::new(0),
                            );
                        }
                    } else {
                        unsafe {
                            A::write(
                                Self::entry_virt_addr(page_list_paddr, page_list.entry_num - 1),
                                PhysAddr::new(0),
                            );
                        }
                    }
                    // 更新当前链表页的统计数据
                    page_list.entry_num -= 1;
                    Self::write_page(page_list_paddr, page_list);
                }
            } else {
                assert!(
                    page_list.entry_num <= Self::BUDDY_ENTRIES,
                    "buddy_free: page_list.entry_num > Self::BUDDY_ENTRIES"
                );

                // 当前第一个page_list没有空间了
                if first_page_list.entry_num == Self::BUDDY_ENTRIES {
                    // 如果当前order是最小的，那么就把这个块当作新的page_list使用
                    let new_page_list_addr = if order == MIN_ORDER {
                        base
                    } else {
                        // 否则分配新的page_list
                        // 请注意，分配之后，有可能当前的entry_num会减1（伙伴块分裂），造成出现整个链表为null的entry数量为Self::BUDDY_ENTRIES+1的情况
                        // 但是不影响，我们在后面插入链表项的时候，会处理这种情况，检查链表中的第2个页是否有空位
                        self.buddy_alloc(PageFrameCount::new(1))
                            .expect("buddy_alloc failed: no enough memory")
                            .0
                    };

                    // 清空这个页面
                    core::ptr::write_bytes(
                        A::phys_2_virt(new_page_list_addr)
                            .expect(
                                "Buddy free: failed to get virt address of [new_page_list_addr]",
                            )
                            .as_ptr::<u8>(),
                        0,
                        1 << order,
                    );
                    assert!(
                        first_page_list_paddr == self.free_area[Self::order2index(order as u8)]
                    );
                    // 初始化新的page_list
                    let new_page_list = PageList::new(0, first_page_list_paddr);
                    Self::write_page(new_page_list_addr, new_page_list);
                    self.free_area[Self::order2index(order as u8)] = new_page_list_addr;
                }

                // 由于上面可能更新了第一个链表页，因此需要重新获取这个值
                let first_page_list_paddr = self.free_area[Self::order2index(order as u8)];
                let first_page_list: PageList<A> = Self::read_page(first_page_list_paddr);

                // 检查第二个page_list是否有空位
                let second_page_list = if first_page_list.next_page.is_null() {
                    None
                } else {
                    Some(Self::read_page::<PageList<A>>(first_page_list.next_page))
                };

                let (paddr, mut page_list) = if let Some(second) = second_page_list {
                    // 第二个page_list有空位
                    // 应当符合之前的假设：还有1个空位
                    assert!(second.entry_num == Self::BUDDY_ENTRIES - 1);

                    (first_page_list.next_page, second)
                } else {
                    // 在第一个page list中分配
                    (first_page_list_paddr, first_page_list)
                };

                // debug!("to write entry, page_list_base={paddr:?}, page_list.entry_num={}, value={base:?}", page_list.entry_num);
                assert!(page_list.entry_num < Self::BUDDY_ENTRIES);
                // 把要归还的块，写入到链表项中
                unsafe { A::write(Self::entry_virt_addr(paddr, page_list.entry_num), base) }
                page_list.entry_num += 1;
                Self::write_page(paddr, page_list);
                return;
            }
            base = min(base, buddy_addr);
            order += 1;
        }
        // 走到这一步，order应该为MAX_ORDER-1
        assert!(order == MAX_ORDER - 1);
    }
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
        let mut free_page_num: usize = 0;
        for index in 0..(MAX_ORDER - MIN_ORDER) {
            let mut pagelist: PageList<A> = Self::read_page(self.free_area[index]);
            loop {
                free_page_num += pagelist.entry_num << index;
                if pagelist.next_page.is_null() {
                    break;
                }
                pagelist = Self::read_page(pagelist.next_page);
            }
        }
        let free = PageFrameCount::new(free_page_num);
        PageFrameUsage::new(self.total - free, self.total)
    }
}

/// 一个用于计算整数的对数的函数，会向下取整。（由于内核不能进行浮点运算，因此需要这个函数）
fn log2(x: usize) -> usize {
    let leading_zeros = x.leading_zeros() as usize;
    let log2x = 63 - leading_zeros;
    return log2x;
}
