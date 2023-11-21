/// @Auther: Kong
/// @Date: 2023-03-27 06:54:08
/// @FilePath: /DragonOS/kernel/src/mm/allocator/bump.rs
/// @Description: bump allocator线性分配器
use super::page_frame::{FrameAllocator, PageFrameCount, PageFrameUsage};
use crate::mm::{MemoryManagementArch, PhysAddr, PhysMemoryArea};
use core::marker::PhantomData;

/// 线性分配器
pub struct BumpAllocator<MMA> {
    // 表示可用物理内存区域的数组。每个 PhysMemoryArea 结构体描述一个物理内存区域的起始地址和大小。
    areas: &'static [PhysMemoryArea],
    // 表示当前分配的物理内存的偏移量.
    offset: usize,
    // 一个占位类型，用于标记 A 类型在结构体中的存在。但是，它并不会占用任何内存空间，因为它的大小为 0。
    phantom: PhantomData<MMA>,
}

/// 为BumpAllocator实现FrameAllocator
impl<MMA: MemoryManagementArch> BumpAllocator<MMA> {
    /// @brief: 创建一个线性分配器
    /// @param Fareas 当前的内存区域
    /// @param offset 当前的偏移量
    /// @return 分配器本身
    pub fn new(areas: &'static [PhysMemoryArea], offset: usize) -> Self {
        Self {
            areas,
            offset,
            phantom: PhantomData,
        }
    }
    // @brief 获取页帧使用情况
    pub fn areas(&self) -> &'static [PhysMemoryArea] {
        return self.areas;
    }
    // @brief 获取当前分配的物理内存的偏移量
    pub fn offset(&self) -> usize {
        return self.offset;
    }

    /// 返回剩余的尚未被分配的物理内存区域
    ///
    /// ## 返回值
    ///
    /// - `result_area`：剩余的尚未被分配的物理内存区域的数组
    /// - `offset_aligned`：返回的第一个物理内存区域内，已经分配的偏移量(相对于物理内存区域的已对齐的起始地址)
    pub fn remain_areas(&self, result_area: &mut [PhysMemoryArea]) -> Option<usize> {
        let mut offset = self.offset();

        let mut ret_offset_aligned = 0;

        let mut res_cnt = 0;

        // 遍历所有的物理内存区域
        for i in 0..self.areas().len() {
            let area = &self.areas()[i];
            // 将area的base地址与PAGE_SIZE对齐，对齐时向上取整
            // let area_base = (area.base.data() + MMA::PAGE_SHIFT) & !(MMA::PAGE_SHIFT);
            let area_base = area.area_base_aligned().data();
            // 将area的末尾地址与PAGE_SIZE对齐，对齐时向下取整
            // let area_end = (area.base.data() + area.size) & !(MMA::PAGE_SHIFT);
            let area_end = area.area_end_aligned().data();

            // 如果offset大于area_end，说明当前的物理内存区域已经分配完了，需要跳到下一个物理内存区域
            if offset >= area_end {
                continue;
            }

            // 如果offset小于area_base ,说明当前的物理内存区域还没有分配过页帧，将offset设置为area_base
            if offset < area_base {
                offset = area_base;
            } else if offset < area_end {
                // 将offset对齐到PAGE_SIZE
                offset = (offset + (MMA::PAGE_SIZE - 1)) & !(MMA::PAGE_SIZE - 1);
            }
            // found
            if offset + 1 * MMA::PAGE_SIZE <= area_end {
                for j in i..self.areas().len() {
                    if self.areas()[j].area_base_aligned() < self.areas()[j].area_end_aligned() {
                        result_area[res_cnt] = self.areas()[j];
                        res_cnt += 1;
                    }
                }
                ret_offset_aligned = offset;
                break;
            }
        }

        let res_cnt = unsafe { Self::arch_remain_areas(result_area, res_cnt) };
        if res_cnt == 0 {
            return None;
        } else {
            return Some(ret_offset_aligned);
        }
    }
}

impl<MMA: MemoryManagementArch> FrameAllocator for BumpAllocator<MMA> {
    /// @brief: 分配count个物理页帧
    /// @param  mut self
    /// @param  count 分配的页帧数量
    /// @return 分配后的物理地址
    unsafe fn allocate(&mut self, count: PageFrameCount) -> Option<(PhysAddr, PageFrameCount)> {
        let mut offset = self.offset();
        // 遍历所有的物理内存区域
        for area in self.areas().iter() {
            // 将area的base地址与PAGE_SIZE对齐，对齐时向上取整
            // let area_base = (area.base.data() + MMA::PAGE_SHIFT) & !(MMA::PAGE_SHIFT);
            let area_base = area.area_base_aligned().data();
            // 将area的末尾地址与PAGE_SIZE对齐，对齐时向下取整
            // let area_end = (area.base.data() + area.size) & !(MMA::PAGE_SHIFT);
            let area_end = area.area_end_aligned().data();

            // 如果offset大于area_end，说明当前的物理内存区域已经分配完了，需要跳到下一个物理内存区域
            if offset >= area_end {
                continue;
            }

            // 如果offset小于area_base ,说明当前的物理内存区域还没有分配过页帧，将offset设置为area_base
            if offset < area_base {
                offset = area_base;
            } else if offset < area_end {
                // 将offset对齐到PAGE_SIZE
                offset = (offset + (MMA::PAGE_SIZE - 1)) & !(MMA::PAGE_SIZE - 1);
            }
            // 如果当前offset到area_end的距离大于等于count.data() * PAGE_SIZE，说明当前的物理内存区域足以分配count个页帧
            if offset + count.data() * MMA::PAGE_SIZE <= area_end {
                let res_page_phys = offset;
                // 将offset增加至分配后的内存
                self.offset = offset + count.data() * MMA::PAGE_SIZE;

                return Some((PhysAddr(res_page_phys), count));
            }
        }
        return None;
    }

    unsafe fn free(&mut self, _address: PhysAddr, _count: PageFrameCount) {
        // TODO: 支持释放页帧
        unimplemented!("BumpAllocator::free not implemented");
    }
    /// @brief: 获取内存区域页帧的使用情况
    /// @param  self
    /// @return 页帧的使用情况
    unsafe fn usage(&self) -> PageFrameUsage {
        let mut total = 0;
        let mut used = 0;
        for area in self.areas().iter() {
            // 将area的base地址与PAGE_SIZE对齐，对其时向上取整
            let area_base = (area.base.data() + MMA::PAGE_SHIFT) & !(MMA::PAGE_SHIFT);
            // 将area的末尾地址与PAGE_SIZE对齐，对其时向下取整
            let area_end = (area.base.data() + area.size) & !(MMA::PAGE_SHIFT);

            total += (area_end - area_base) >> MMA::PAGE_SHIFT;
            // 如果offset大于area_end，说明当前物理区域被分配完，都需要加到used中
            if self.offset >= area_end {
                used += (area_end - area_base) >> MMA::PAGE_SHIFT;
            } else if self.offset < area_base {
                // 如果offset小于area_base，说明当前物理区域还没有分配过页帧，都不需要加到used中
                continue;
            } else {
                used += (self.offset - area_base) >> MMA::PAGE_SHIFT;
            }
        }
        let frame = PageFrameUsage::new(PageFrameCount::new(used), PageFrameCount::new(total));
        return frame;
    }
}
