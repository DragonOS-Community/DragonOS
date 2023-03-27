use crate::mm::page_frame::{FrameAllocator, PageFrameUsage, PageFrameCount};
use crate::mm::{MemoryManagementArch, PhysAddr, PhysMemoryArea};
use core::marker::PhantomData;
// 线性分配器的实现
pub struct BumpAllocator<MMA> {
    // 表示可用物理内存区域的数组。每个 PhysMemoryArea 结构体描述一个物理内存区域的起始地址和大小。
    areas: &'static [PhysMemoryArea],
    // 表示当前分配的物理内存的偏移量.
    offset: usize,
    // 一个占位类型，用于标记 A 类型在结构体中的存在。但是，它并不会占用任何内存空间，因为它的大小为 0。
    phantom: PhantomData<MMA>,
}
// 为BumpAllocator实现FrameAllocator
impl<MMA: MemoryManagementArch> BumpAllocator<MMA> {
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
}

impl<MMA: MemoryManagementArch> FrameAllocator for BumpAllocator<MMA> {
    unsafe fn allocate(&mut self, count: PageFrameCount) -> Option<PhysAddr> {
        //TODO: 支持分配多个页帧
        // 目前只支持分配一个页帧，所以如果count.data不等于1，直接返回None
        if count.data() != 1 {
            return None;
        }
        let mut offset = self.offset;
        // 遍历所有的物理内存区域
        for area in self.areas.iter() {
            // 如果offset小于area.size，说明当前的物理内存区域还有空闲的页帧
            if offset < area.size {
                let page_phys = area.base.add(offset);
                let page_virt = MMA::phys_2_virt(page_phys);
                // 将page_virt从Option中取出来
                let page_virt =
                    page_virt.expect("BumpAllocator::allocate: invalid physical address");
                MMA::write_bytes(page_virt, 0, MMA::PAGE_SIZE);
                self.offset += MMA::PAGE_SIZE;
                return Some(page_phys);
            }
            // 如果offset大于area.size，说明当前的物理内存区域已经分配完了，需要跳到下一个物理内存区域
            offset -= area.size;
        }
        return None;
    }

    unsafe fn free(&mut self, _address: PhysAddr, _count: PageFrameCount) {
        // TODO: 支持释放页帧
        unimplemented!("BumpAllocator::free not implemented");
    }

    unsafe fn usage(&self) -> PageFrameUsage {
        let mut total = 0;
        for area in self.areas.iter() {
            total += area.size >> MMA::PAGE_SHIFT;
        }
        let used = self.offset >> MMA::PAGE_SHIFT;
        let frame = PageFrameUsage::new(PageFrameCount::new(used), PageFrameCount::new(total));
        return frame;
    }
}
