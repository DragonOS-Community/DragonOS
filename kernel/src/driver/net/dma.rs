use crate::arch::mm::kernel_page_flags;

use crate::arch::MMArch;

use crate::mm::kernel_mapper::KernelMapper;
use crate::mm::page::EntryFlags;
use crate::mm::{
    allocator::page_frame::{
        allocate_page_frames, deallocate_page_frames, PageFrameCount, PhysPageFrame,
    },
    MemoryManagementArch, PhysAddr, VirtAddr,
};
use core::ptr::NonNull;
const PAGE_SIZE: usize = 4096;
/// @brief 申请用于DMA的内存页
/// @param pages 页数（4k一页）
/// @return PhysAddr 获得的内存页的初始物理地址
pub fn dma_alloc(pages: usize) -> (usize, NonNull<u8>) {
    let page_num = PageFrameCount::new(
        (pages * PAGE_SIZE)
            .div_ceil(MMArch::PAGE_SIZE)
            .next_power_of_two(),
    );
    unsafe {
        let (paddr, count) = allocate_page_frames(page_num).expect("e1000e: alloc page failed");
        let virt = MMArch::phys_2_virt(paddr).unwrap();
        // 清空这块区域，防止出现脏数据
        core::ptr::write_bytes(virt.data() as *mut u8, 0, count.data() * MMArch::PAGE_SIZE);

        let dma_flags: EntryFlags<MMArch> = EntryFlags::mmio_flags();

        let mut kernel_mapper = KernelMapper::lock();
        let kernel_mapper = kernel_mapper.as_mut().unwrap();
        let flusher = kernel_mapper
            .remap(virt, dma_flags)
            .expect("e1000e: remap failed");
        flusher.flush();
        return (
            paddr.data(),
            NonNull::new(MMArch::phys_2_virt(paddr).unwrap().data() as _).unwrap(),
        );
    }
}
/// @brief 释放用于DMA的内存页
/// @param paddr 起始物理地址 pages 页数（4k一页）
/// @return i32 0表示成功
pub unsafe fn dma_dealloc(paddr: usize, vaddr: NonNull<u8>, pages: usize) -> i32 {
    let page_count = PageFrameCount::new(
        (pages * PAGE_SIZE)
            .div_ceil(MMArch::PAGE_SIZE)
            .next_power_of_two(),
    );

    // 恢复页面属性
    let vaddr = VirtAddr::new(vaddr.as_ptr() as usize);
    let mut kernel_mapper = KernelMapper::lock();
    let kernel_mapper = kernel_mapper.as_mut().unwrap();
    let flusher = kernel_mapper
        .remap(vaddr, kernel_page_flags(vaddr))
        .expect("e1000e: remap failed");
    flusher.flush();

    unsafe {
        deallocate_page_frames(PhysPageFrame::new(PhysAddr::new(paddr)), page_count);
    }
    return 0;
}
