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
use virtio_drivers::{BufferDirection, Hal, PAGE_SIZE};

pub struct HalImpl;
unsafe impl Hal for HalImpl {
    /// @brief 申请用于DMA的内存页
    /// @param pages 页数（4k一页）
    /// @return PhysAddr 获得的内存页的初始物理地址
    fn dma_alloc(
        pages: usize,
        _direction: BufferDirection,
    ) -> (virtio_drivers::PhysAddr, NonNull<u8>) {
        let page_num = PageFrameCount::new(
            (pages * PAGE_SIZE)
                .div_ceil(MMArch::PAGE_SIZE)
                .next_power_of_two(),
        );
        unsafe {
            let (paddr, count) =
                allocate_page_frames(page_num).expect("VirtIO Impl: alloc page failed");
            let virt = MMArch::phys_2_virt(paddr).unwrap();
            // 清空这块区域，防止出现脏数据
            core::ptr::write_bytes(virt.data() as *mut u8, 0, count.data() * MMArch::PAGE_SIZE);

            let dma_flags: EntryFlags<MMArch> = EntryFlags::mmio_flags();

            let mut kernel_mapper = KernelMapper::lock();
            let kernel_mapper = kernel_mapper.as_mut().unwrap();
            let flusher = kernel_mapper
                .remap(virt, dma_flags)
                .expect("VirtIO Impl: remap failed");
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
    unsafe fn dma_dealloc(
        paddr: virtio_drivers::PhysAddr,
        vaddr: NonNull<u8>,
        pages: usize,
    ) -> i32 {
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
            .expect("VirtIO Impl: remap failed");
        flusher.flush();

        unsafe {
            deallocate_page_frames(PhysPageFrame::new(PhysAddr::new(paddr)), page_count);
        }
        return 0;
    }
    /// @brief mmio物理地址转换为虚拟地址，不需要使用
    /// @param paddr 起始物理地址
    /// @return NonNull<u8> 虚拟地址的指针
    unsafe fn mmio_phys_to_virt(paddr: virtio_drivers::PhysAddr, _size: usize) -> NonNull<u8> {
        NonNull::new((MMArch::phys_2_virt(PhysAddr::new(paddr))).unwrap().data() as _).unwrap()
    }
    /// @brief 与真实物理设备共享
    /// @param buffer 要共享的buffer _direction：设备到driver或driver到设备
    /// @return buffer在内存中的物理地址
    unsafe fn share(
        buffer: NonNull<[u8]>,
        _direction: BufferDirection,
    ) -> virtio_drivers::PhysAddr {
        let vaddr = VirtAddr::new(buffer.as_ptr() as *mut u8 as usize);
        //debug!("virt:{:x}", vaddr);
        // Nothing to do, as the host already has access to all memory.
        return MMArch::virt_2_phys(vaddr).unwrap().data();
    }
    /// @brief 停止共享（让主机可以访问全部内存的话什么都不用做）
    unsafe fn unshare(
        _paddr: virtio_drivers::PhysAddr,
        _buffer: NonNull<[u8]>,
        _direction: BufferDirection,
    ) {
        // Nothing to do, as the host already has access to all memory and we didn't copy the buffer
        // anywhere else.
    }
}
