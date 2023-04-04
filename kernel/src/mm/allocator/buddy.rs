use crate::mm;
/// @Auther: Kong
/// @Date: 2023-03-28 16:03:47
/// @FilePath: /DragonOS/kernel/src/mm/allocator/buddycopy.rs
/// @Description:
use alloc::collections::LinkedList;

use crate::mm::allocator::bump::BumpAllocator;
use crate::mm::allocator::page_frame::{FrameAllocator, PageFrameCount, PageFrameUsage};
use crate::mm::{MemoryManagementArch, PhysAddr, VirtAddr};
use core::{marker::PhantomData, mem};

// 一个全局变量MAX_ORDER，表示最大的阶数
const MAX_ORDER: usize = 11;

// 存放每个entry页的使用情况
#[repr(transparent)]
struct BuddyUsage(u8);

/// @brief: 用来表示 buddy 算法中的一个 buddy 块，整体存放在area的头部
// 这种方式会出现对齐问题
// #[repr(packed)]
#[repr(C)]
pub struct BuddyEntry<A> {
    // 代表的页的起始地址
    base: PhysAddr,
    // entry的阶数
    order: usize,
    // entry是否在Buddy中使用
    pg_buddy: usize,
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
            pg_buddy: 0,
            phantom: PhantomData,
        }
    }
}

pub struct BuddyAllocator<A> {
    // buddy表的虚拟地址
    table_virt: VirtAddr,
    // 使用free_area来表示 MAX_ORDER 个阶数的空闲数组，每个数组元素都是一个链表，链表的每个元素都是一个BuddyEntry
    free_area: [LinkedList<BuddyEntry<A>>; MAX_ORDER],
    phantom: PhantomData<A>,
}

impl<A: MemoryManagementArch> BuddyAllocator<A> {
    const BUDDY_ENTRIES: usize = A::PAGE_SIZE / mem::size_of::<BuddyEntry<A>>();

    pub unsafe fn new(mut bump_allocator: BumpAllocator<A>) -> Option<Self> {
        // 分配一个页用于存储 buddy 算法的数据结构
        let table_phys = bump_allocator.allocate_one()?;
        let table_virt = A::phys_2_virt(table_phys);
        let table_virt = table_virt?;
        // 将申请到的内存全部分配为 BuddyEntry<A> 类型
        for i in 0..Self::BUDDY_ENTRIES {
            let virt = table_virt.add(i * mem::size_of::<BuddyEntry<A>>());
            A::write(virt, BuddyEntry::<A>::empty());
        }
        // 初始化free_area

        let free_area = Default::default();

        let mut allocator = Self {
            table_virt,
            free_area,
            phantom: PhantomData,
        };
        // TODO 此处在分配时，应该对齐到最大的阶数，即1<<MAX_ORDER
        for old_area in bump_allocator.areas().iter() {
            let mut area = old_area.clone();
            // 如果offset大于area的起始地址，那么需要跳过offset的大小
            if bump_allocator.offset() > area.base.data() {
                area.base = area.base.add(bump_allocator.offset());
                area.size -= bump_allocator.offset();
            }
            // 如果area的大小大于MAX_ORDER，那么将area分割为多个area
            while area.size > (1 << MAX_ORDER) {
                let mut new_area = area.clone();
                new_area.size = 1 << MAX_ORDER;
                area.base = area.base.add(1 << MAX_ORDER);
                area.size -= 1 << MAX_ORDER;
                allocator.add_area(new_area);
            }
            allocator.add_area(area);
        }

        Some(allocator)
    }

    /// @brief: 将一个area添加到free_area中
    /// @param {type}
    /// @area: 要添加的area
    pub fn add_area(&mut self, area: mm::PhysMemoryArea) {
        // 计算area的阶数
        let order = area.size >> A::PAGE_SHIFT;
        // 计算area的起始地址
        let base = area.base;
        let pg_buddy = 0 as usize;

        self.free_area[order].push_back(BuddyEntry {
            base,
            order,
            pg_buddy,
            phantom: PhantomData,
        });
    }
    /// @brief: 移除链表中的buddy
    /// @param {*} self
    /// @param {BuddyEntry} entry
    /// @param {BuddyEntry} buddy
    /// @return {*}
    pub fn remove(&mut self, buddy: BuddyEntry<A>) {
        let order = buddy.order;
        let mut count = 0;
        // 在迭代free_area时使用count统计次数
        for i in self.free_area[order].iter_mut() {
            // 如果i的起始地址等于entry的伙伴的起始地址，那么就将i从free_area中移除
            if i.base.data() == buddy.base.data() {
                break;
            }
            count += 1;
        }
        let mut split_list = self.free_area[order].split_off(count);
        split_list.pop_front();
        self.free_area[order].append(&mut split_list);
    }
}

impl<A: MemoryManagementArch> FrameAllocator for BuddyAllocator<A> {
    unsafe fn allocate(&mut self, count: PageFrameCount) -> Option<PhysAddr> {
        // 如果table_virt为0，说明buddy还没有初始化，那么就直接返回None
        if self.table_virt.data() == 0 {
            return None;
        }

        // 计算要分配的页的阶数
        let mut order = 0;
        while (1 << order) < count.data() {
            order += 1;
        }

        // 从free_area中找到第一个能够满足要求的area
        let mut entry = self.free_area[order].pop_front();
        while entry.is_none() {
            order += 1;
            if order >= MAX_ORDER {
                return None;
            }
            entry = self.free_area[order].pop_front();
        }
        let mut entry = entry.unwrap();

        // 如果entry的阶数大于要分配的阶数，那么将entry分割为两个entry
        while entry.order > order {
            entry.order -= 1;
            let new_entry = BuddyEntry {
                base: entry.base.add(1 << (entry.order + A::PAGE_SHIFT)),
                order: entry.order,
                pg_buddy: 0,
                phantom: PhantomData,
            };
            self.free_area[entry.order].push_back(new_entry);
        }

        // 更新entry的使用情况
        let start_page = entry.base.data() >> A::PAGE_SHIFT;
        // 设置start_page到start_page+count.data()的entry的pg_buddy位为1
        for i in start_page..(start_page + count.data()) {
            let virt = self.table_virt.add(i * mem::size_of::<BuddyEntry<A>>());
            let mut entry = A::read::<BuddyEntry<A>>(virt);
            entry.pg_buddy = 1;
            A::write(virt, entry);
        }
        Some(entry.base)
    }

    unsafe fn free(&mut self, base: PhysAddr, count: PageFrameCount) {
        // 计算base对应的entry的虚拟地址
        let start_page = base.data() >> A::PAGE_SHIFT;
        let virt = self
            .table_virt
            .add(start_page * mem::size_of::<BuddyEntry<A>>());
        // 读取base对应的entry
        let mut entry = A::read::<BuddyEntry<A>>(virt);
        // 如果entry的pg_buddy位为0，说明entry已经被释放了，那么就直接返回
        if entry.pg_buddy == 0 {
            return;
        }
        // 将entry的pg_buddy位设置为0
        entry.pg_buddy = 0;
        A::write(virt, entry);

        // 如果entry的阶数小于MAX_ORDER，那么就将entry合并到buddy中
        while entry.order < MAX_ORDER {
            // 获取entry的伙伴的页号
            let buddy_page = if entry.base.data() & (1 << entry.order) == 0 {
                start_page + 1
            } else {
                start_page - 1
            };

            let buddy_virt = self
                .table_virt
                .add(buddy_page * mem::size_of::<BuddyEntry<A>>());
            // 读取entry的buddy
            let mut buddy = A::read::<BuddyEntry<A>>(buddy_virt);
            // 如果entry的buddy的阶数不等于entry的阶数，或者entry的buddy的pg_buddy位为1，那么就退出循环
            if buddy.order != entry.order || buddy.pg_buddy == 1 {
                break;
            }
            // 将entry的伙伴的pg_buddy位设置为0
            buddy.pg_buddy = 0;
            A::write(buddy_virt, buddy);

            // 遍历free_area[entry.order]中的链表，移除buddy
            self.remove(buddy);
            // 将entry的buddy从free_area中移除，因为entry和entry的buddy已经合并了
            self.remove(entry);
            // 将entry的阶数加1
            entry.order += 1;
            // 如果entry的起始地址大于entry的buddy的起始地址，那么就将entry的起始地址设置为entry的buddy的起始地址
            if entry.base.data() > buddy.base.data() {
                entry.base = buddy
                    .base
                    .add((1 << (entry.order + A::PAGE_SHIFT)) - (1 << A::PAGE_SHIFT));
            }

            // 将entry添加到free_area中
            self.free_area[entry.order].push_back(entry);
        }
    }
    unsafe fn usage(&self) -> PageFrameUsage {
        let mut total = 0;
        let mut used = 0;
        for i in 0..MAX_ORDER {
            total += self.free_area[i].len() * (1 << i);
            for j in self.free_area[i].iter() {
                if j.pg_buddy == 1 {
                    used += 1 << i;
                }
            }
        }
        let frame = PageFrameUsage::new(PageFrameCount::new(used), PageFrameCount::new(total));
        return frame;
    }
}
