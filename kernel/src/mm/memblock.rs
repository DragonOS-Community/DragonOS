use core::intrinsics::unlikely;

use log::error;
use system_error::SystemError;

use crate::libs::{
    align::{page_align_down, page_align_up},
    spinlock::{SpinLock, SpinLockGuard},
};

use super::{PhysAddr, PhysMemoryArea};

pub const INITIAL_MEMORY_REGIONS_NUM: usize = 128;

/// 初始内存区域
static MEM_BLOCK_MANAGER: MemBlockManager = MemBlockManager::new();

#[inline(always)]
pub fn mem_block_manager() -> &'static MemBlockManager {
    &MEM_BLOCK_MANAGER
}

/// 内存区域管理器
#[derive(Debug)]
pub struct MemBlockManager {
    inner: SpinLock<InnerMemBlockManager>,
}

#[derive(Debug)]
pub struct InnerMemBlockManager {
    /// 初始内存区域
    ///
    /// 用于记录内核启动时的内存布局, 这些区域保持升序、不重叠
    initial_memory_regions: [PhysMemoryArea; INITIAL_MEMORY_REGIONS_NUM],
    initial_memory_regions_num: usize,
}

impl MemBlockManager {
    #[allow(dead_code)]
    pub const MIN_MEMBLOCK_ADDR: PhysAddr = PhysAddr::new(0);
    #[allow(dead_code)]
    pub const MAX_MEMBLOCK_ADDR: PhysAddr = PhysAddr::new(usize::MAX);
    const fn new() -> Self {
        Self {
            inner: SpinLock::new(InnerMemBlockManager {
                initial_memory_regions: [PhysMemoryArea::DEFAULT; INITIAL_MEMORY_REGIONS_NUM],
                initial_memory_regions_num: 0,
            }),
        }
    }

    /// 添加内存区域
    ///
    /// 如果添加的区域与已有区域有重叠，会将重叠的区域合并
    #[allow(dead_code)]
    pub fn add_block(&self, base: PhysAddr, size: usize) -> Result<(), SystemError> {
        let r = self.add_range(base, size, MemoryAreaAttr::empty());
        return r;
    }

    /// 添加内存区域
    ///
    /// 如果添加的区域与已有区域有重叠，会将重叠的区域合并
    fn add_range(
        &self,
        base: PhysAddr,
        size: usize,
        flags: MemoryAreaAttr,
    ) -> Result<(), SystemError> {
        if size == 0 {
            return Ok(());
        }
        let mut inner = self.inner.lock();
        if inner.initial_memory_regions_num >= INITIAL_MEMORY_REGIONS_NUM {
            panic!("Too many memory regions!");
        }

        let block = PhysMemoryArea::new(base, size, MemoryAreaAttr::empty());
        // 特判第一个区域
        if inner.initial_memory_regions_num == 0 {
            inner.initial_memory_regions[0] = block;
            inner.initial_memory_regions_num += 1;
            return Ok(());
        }

        // 先计算需要添加的区域数量
        let blocks_to_add = self
            .do_add_block(&mut inner, block, false, flags)
            .expect("Failed to count blocks to add!");

        if inner.initial_memory_regions_num + blocks_to_add > INITIAL_MEMORY_REGIONS_NUM {
            error!("Too many memory regions!");
            return Err(SystemError::ENOMEM);
        }

        // 然后添加区域
        self.do_add_block(&mut inner, block, true, flags)
            .expect("Failed to add block!");

        return Ok(());
    }

    fn do_add_block(
        &self,
        inner: &mut SpinLockGuard<'_, InnerMemBlockManager>,
        block: PhysMemoryArea,
        insert: bool,
        flags: MemoryAreaAttr,
    ) -> Result<usize, SystemError> {
        let mut base = block.base;
        let end = block.base + block.size;
        let mut i = 0;
        let mut start_index = -1;
        let mut end_index = -1;

        let mut num_to_add = 0;

        while i < inner.initial_memory_regions_num {
            let range_base = inner.initial_memory_regions[i].base;
            let range_end =
                inner.initial_memory_regions[i].base + inner.initial_memory_regions[i].size;

            if range_base >= end {
                break;
            }
            if range_end <= base {
                i += 1;
                continue;
            }

            // 有重叠

            if range_base > base {
                num_to_add += 1;
                if insert {
                    if start_index == -1 {
                        start_index = i as isize;
                    }
                    end_index = (i + 1) as isize;
                    self.do_insert_area(inner, i, base, range_base - base, flags);
                    i += 1;
                }
            }

            i += 1;
            base = core::cmp::min(range_end, end);
        }

        if base < end {
            num_to_add += 1;
            if insert {
                if start_index == -1 {
                    start_index = i as isize;
                }
                end_index = (i + 1) as isize;
                self.do_insert_area(inner, i, base, end - base, flags);
            }
        }

        if num_to_add == 0 {
            return Ok(0);
        }

        if insert {
            self.do_merge_blocks(inner, start_index, end_index);
        }
        return Ok(num_to_add);
    }

    fn do_insert_area(
        &self,
        inner: &mut SpinLockGuard<'_, InnerMemBlockManager>,
        index: usize,
        base: PhysAddr,
        size: usize,
        flags: MemoryAreaAttr,
    ) {
        let copy_elements = inner.initial_memory_regions_num - index;
        inner
            .initial_memory_regions
            .copy_within(index..index + copy_elements, index + 1);
        inner.initial_memory_regions[index] = PhysMemoryArea::new(base, size, flags);
        inner.initial_memory_regions_num += 1;
    }

    fn do_merge_blocks(
        &self,
        inner: &mut SpinLockGuard<'_, InnerMemBlockManager>,
        start_index: isize,
        mut end_index: isize,
    ) {
        let mut i = 0;
        if start_index > 0 {
            i = start_index - 1;
        }
        end_index = core::cmp::min(end_index, inner.initial_memory_regions_num as isize - 1);

        while i < end_index {
            {
                let next_base = inner.initial_memory_regions[(i + 1) as usize].base;
                let next_size = inner.initial_memory_regions[(i + 1) as usize].size;
                let next_flags = inner.initial_memory_regions[(i + 1) as usize].flags;
                let this = &mut inner.initial_memory_regions[i as usize];

                if this.base + this.size != next_base || this.flags != next_flags {
                    if unlikely(this.base + this.size > next_base) {
                        panic!("this->base + this->size > next->base");
                    }
                    i += 1;
                    continue;
                }
                this.size += next_size;
            }
            // 移动后面的区域
            let copy_elements = inner.initial_memory_regions_num - (i + 2) as usize;
            inner.initial_memory_regions.copy_within(
                (i + 2) as usize..(i as usize + 2 + copy_elements),
                (i + 1) as usize,
            );

            inner.initial_memory_regions_num -= 1;
            end_index -= 1;
        }
    }

    /// 移除内存区域
    ///
    /// 如果移除的区域与已有区域有重叠，会将重叠的区域分割
    #[allow(dead_code)]
    pub fn remove_block(&self, base: PhysAddr, size: usize) -> Result<(), SystemError> {
        if size == 0 {
            return Ok(());
        }
        let mut inner = self.inner.lock();
        if inner.initial_memory_regions_num == 0 {
            return Ok(());
        }

        let (start_index, end_index) = self
            .isolate_range(&mut inner, base, size)
            .expect("Failed to isolate range!");

        for i in (start_index..end_index).rev() {
            self.do_remove_region(&mut inner, i);
        }
        return Ok(());
    }

    fn do_remove_region(&self, inner: &mut SpinLockGuard<'_, InnerMemBlockManager>, index: usize) {
        let copy_elements = inner.initial_memory_regions_num - index - 1;
        inner
            .initial_memory_regions
            .copy_within(index + 1..index + 1 + copy_elements, index);

        inner.initial_memory_regions_num -= 1;

        if inner.initial_memory_regions_num == 0 {
            inner.initial_memory_regions[0].base = PhysAddr::new(0);
            inner.initial_memory_regions[0].size = 0;
        }
    }

    /// 在一个内存块管理器中找到一个物理地址范围内的
    /// 空闲块，并隔离出所需的内存大小
    ///
    /// ## 返回值
    ///
    /// - Ok((start_index, end_index)) 表示成功找到了一个连续的内存区域来满足所需的 size。这里：
    ///     - start_index 是指定的起始内存区域的索引。
    ///     - end_index 是指定的结束内存区域的索引，它实际上不包含在返回的连续区域中，但它标志着下一个可能的不连续区域的开始。
    /// - Err(SystemError) 则表示没有找到足够的空间来满足请求的 size，可能是因为内存区域不足或存在其他系统错误
    fn isolate_range(
        &self,
        inner: &mut SpinLockGuard<'_, InnerMemBlockManager>,
        base: PhysAddr,
        size: usize,
    ) -> Result<(usize, usize), SystemError> {
        let end = base + size;

        let mut idx = 0;

        let mut start_index = 0;
        let mut end_index = 0;

        if size == 0 {
            return Ok((0, 0));
        }

        while idx < inner.initial_memory_regions_num {
            let range_base = inner.initial_memory_regions[idx].base;
            let range_end = range_base + inner.initial_memory_regions[idx].size;

            if range_base >= end {
                break;
            }
            if range_end <= base {
                idx = idx.checked_add(1).unwrap_or(0);
                continue;
            }

            if range_base < base {
                // regions[idx] intersects from below
                inner.initial_memory_regions[idx].base = base;
                inner.initial_memory_regions[idx].size -= base - range_base;
                self.do_insert_area(
                    inner,
                    idx,
                    range_base,
                    base - range_base,
                    inner.initial_memory_regions[idx].flags,
                );
            } else if range_end > end {
                // regions[idx] intersects from above
                inner.initial_memory_regions[idx].base = end;
                inner.initial_memory_regions[idx].size -= end - range_base;

                self.do_insert_area(
                    inner,
                    idx,
                    range_base,
                    end - range_base,
                    inner.initial_memory_regions[idx].flags,
                );
                if idx == 0 {
                    idx = usize::MAX;
                } else {
                    idx -= 1;
                }
            } else {
                // regions[idx] is inside the range, record it
                if end_index == 0 {
                    start_index = idx;
                }
                end_index = idx + 1;
            }

            idx = idx.checked_add(1).unwrap_or(0);
        }

        return Ok((start_index, end_index));
    }

    /// mark_nomap - 用`MemoryAreaAttr::NOMAP`标志标记内存区域
    ///
    /// ## 参数
    ///
    /// - base: 区域的物理基地址
    /// - size: 区域的大小
    ///
    /// 使用`MemoryAreaAttr::NOMAP`标志标记的内存区域将不会被添加到物理内存的直接映射中。这些区域仍然会被内存映射所覆盖。内存映射中代表NOMAP内存帧的struct page将被PageReserved()。
    /// 注意：如果被标记为`MemoryAreaAttr::NOMAP`的内存是从memblock分配的，调用者必须忽略该内存
    pub fn mark_nomap(&self, base: PhysAddr, size: usize) -> Result<(), SystemError> {
        return self.set_or_clear_flags(base, size, true, MemoryAreaAttr::NOMAP);
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/mm/memblock.c?fi=memblock_mark_mirror#940
    pub fn mark_mirror(&self, base: PhysAddr, size: usize) -> Result<(), SystemError> {
        return self.set_or_clear_flags(base, size, true, MemoryAreaAttr::MIRROR);
    }

    fn set_or_clear_flags(
        &self,
        mut base: PhysAddr,
        mut size: usize,
        set: bool,
        flags: MemoryAreaAttr,
    ) -> Result<(), SystemError> {
        let rsvd_base = PhysAddr::new(page_align_down(base.data()));
        size = page_align_up(size + base.data() - rsvd_base.data());
        base = rsvd_base;

        let mut inner = self.inner.lock();
        let (start_index, end_index) = self.isolate_range(&mut inner, base, size)?;
        for i in start_index..end_index {
            if set {
                inner.initial_memory_regions[i].flags |= flags;
            } else {
                inner.initial_memory_regions[i].flags &= !flags;
            }
        }

        let num = inner.initial_memory_regions_num as isize;
        self.do_merge_blocks(&mut inner, 0, num);
        return Ok(());
    }

    /// 标记内存区域为保留区域
    pub fn reserve_block(&self, base: PhysAddr, size: usize) -> Result<(), SystemError> {
        return self.set_or_clear_flags(base, size, true, MemoryAreaAttr::RESERVED);
    }

    /// 判断[base, base+size)与已有区域是否有重叠
    pub fn is_overlapped(&self, base: PhysAddr, size: usize) -> bool {
        let inner = self.inner.lock();
        return self.do_is_overlapped(base, size, false, &inner);
    }

    /// 判断[base, base+size)与已有Reserved区域是否有重叠
    pub fn is_overlapped_with_reserved(&self, base: PhysAddr, size: usize) -> bool {
        let inner = self.inner.lock();
        return self.do_is_overlapped(base, size, true, &inner);
    }

    fn do_is_overlapped(
        &self,
        base: PhysAddr,
        size: usize,
        require_reserved: bool,
        inner: &SpinLockGuard<'_, InnerMemBlockManager>,
    ) -> bool {
        let mut res = false;
        for i in 0..inner.initial_memory_regions_num {
            if require_reserved
                && !inner.initial_memory_regions[i]
                    .flags
                    .contains(MemoryAreaAttr::RESERVED)
            {
                // 忽略非保留区域
                continue;
            }

            let range_base = inner.initial_memory_regions[i].base;
            let range_end = range_base + inner.initial_memory_regions[i].size;
            if (base >= range_base && base < range_end)
                || (base + size > range_base && base + size <= range_end)
                || (base <= range_base && base + size >= range_end)
            {
                res = true;
                break;
            }
        }

        return res;
    }

    /// 生成迭代器
    pub fn to_iter(&self) -> MemBlockIter {
        let inner = self.inner.lock();
        return MemBlockIter {
            inner,
            index: 0,
            usable_only: false,
        };
    }

    /// 生成迭代器，迭代所有可用的物理内存区域
    pub fn to_iter_available(&self) -> MemBlockIter {
        let inner = self.inner.lock();
        return MemBlockIter {
            inner,
            index: 0,
            usable_only: true,
        };
    }

    /// 获取初始内存区域数量
    pub fn total_initial_memory_regions(&self) -> usize {
        let inner = self.inner.lock();
        return inner.initial_memory_regions_num;
    }

    /// 根据索引获取初始内存区域
    pub fn get_initial_memory_region(&self, index: usize) -> Option<PhysMemoryArea> {
        let inner = self.inner.lock();
        return inner.initial_memory_regions.get(index).copied();
    }
}

pub struct MemBlockIter<'a> {
    inner: SpinLockGuard<'a, InnerMemBlockManager>,
    index: usize,
    usable_only: bool,
}

#[allow(dead_code)]
impl MemBlockIter<'_> {
    /// 获取内存区域数量
    pub fn total_num(&self) -> usize {
        self.inner.initial_memory_regions_num
    }

    /// 获取指定索引的内存区域
    pub fn get_area(&self, index: usize) -> &PhysMemoryArea {
        &self.inner.initial_memory_regions[index]
    }

    /// 获取当前索引
    pub fn current_index(&self) -> usize {
        self.index
    }
}

impl Iterator for MemBlockIter<'_> {
    type Item = PhysMemoryArea;

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < self.inner.initial_memory_regions_num {
            if self.usable_only
                && !self.inner.initial_memory_regions[self.index]
                    .flags
                    .is_empty()
            {
                self.index += 1;
                if self.index >= self.inner.initial_memory_regions_num {
                    return None;
                }
                continue;
            }
            break;
        }
        if self.index >= self.inner.initial_memory_regions_num {
            return None;
        }
        let ret = self.inner.initial_memory_regions[self.index];
        self.index += 1;
        return Some(ret);
    }
}

bitflags! {
    /// 内存区域属性
    #[allow(clippy::bad_bit_mask)]
    pub struct MemoryAreaAttr: u32 {
        /// No special request
        const NONE = 0x0;
        /// Hotpluggable region
        const HOTPLUG = (1 << 0);
        /// Mirrored region
        const MIRROR = (1 << 1);
        /// do not add to kenrel direct mapping
        const NOMAP = (1 << 2);
        /// Always detected via a driver
        const DRIVER_MANAGED = (1 << 3);
        /// Memory is reserved
        const RESERVED = (1 << 4);
    }
}
