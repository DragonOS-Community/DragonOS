use crate::mm::page_frame::{FrameAllocator, PageFrameCount, PageFrameUsage};
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
        let mut offset = self.offset();
        // 遍历所有的物理内存区域
        for area in self.areas().iter() {
            // 将area的base地址与PAGE_SIZE对齐，对其时向上取整
            let area_base = (area.base.data() + MMA::PAGE_SIZE - 1) & !(MMA::PAGE_SIZE - 1);
            // 将area的末尾地址与PAGE_SIZE对齐，对其时向下取整
            let area_end = (area.base.data() + area.size) & !(MMA::PAGE_SIZE - 1);

            // 如果offset大于area_end，说明当前的物理内存区域已经分配完了，需要跳到下一个物理内存区域
            if offset >= area_end {
                continue;
            }

            // 如果offset小于area_base 或者不小于area_base但小于area_end，说明当前的物理内存区域还没有分配过页帧，需要将offset调整到area_base
            if offset < area_base || offset < area_end {
                offset = area_base;
            }
            // 如果当前offset到area_end的距离大于等于count.data() * PAGE_SIZE，说明当前的物理内存区域足以分配count个页帧
            if offset + count.data() * MMA::PAGE_SIZE <= area_end {
                let res_page_phys = offset;
                let page_phys = area.base.add(count.data() * MMA::PAGE_SIZE);
                // 将page_phys转换为虚拟地址
                let page_virt = MMA::phys_2_virt(page_phys);
                // 将page_virt从Option中取出来
                let page_virt =
                    page_virt.expect("BumpAllocator::allocate: invalid physical address");
                // 将page_virt对应的物理页清零
                MMA::write_bytes(page_virt, 0, count.data() * MMA::PAGE_SIZE);
                // 将offset增加至分配后的内存
                self.offset = offset + count.data() * MMA::PAGE_SIZE;

                return Some(PhysAddr(res_page_phys));
            }
        }
        return None;
    }

    unsafe fn free(&mut self, _address: PhysAddr, _count: PageFrameCount) {
        // TODO: 支持释放页帧
        unimplemented!("BumpAllocator::free not implemented");
    }

    unsafe fn usage(&self) -> PageFrameUsage {
        let mut total = 0;
        let mut used = 0;
        for area in self.areas().iter() {
            // 将area的base地址与PAGE_SIZE对齐，对其时向上取整
            let area_base = (area.base.data() + MMA::PAGE_SIZE - 1) & !(MMA::PAGE_SIZE - 1);
            // 将area的末尾地址与PAGE_SIZE对齐，对其时向下取整
            let area_end = (area.base.data() + area.size) & !(MMA::PAGE_SIZE - 1);

            total += (area_end - area_base) >> MMA::PAGE_SHIFT;
            // 如果offset大于area_end，说明当前物理区域被分配完，都需要加到used中
            if self.offset >= area_end {
                used += (area_end - area_base) >> MMA::PAGE_SHIFT;
            }
            else{
                used += (self.offset - area_base) >> MMA::PAGE_SHIFT;
            }
        }
        let frame = PageFrameUsage::new(PageFrameCount::new(used), PageFrameCount::new(total));
        return frame;
    }
}
