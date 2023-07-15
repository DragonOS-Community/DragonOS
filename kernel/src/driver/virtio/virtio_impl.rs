/// 为virtio-drivers库提供的操作系统接口
use crate::include::bindings::bindings::{
    memory_management_struct, Page, PAGE_2M_SHIFT, PAGE_2M_SIZE,
    PAGE_OFFSET, PAGE_SHARED, ZONE_NORMAL,
};

use crate::arch::MMArch;
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
        let page_num =
            PageFrameCount::new(((pages * PAGE_SIZE + MMArch::PAGE_SIZE - 1) / MMArch::PAGE_SIZE).next_power_of_two());
        unsafe {
            let (paddr, _count) =
                allocate_page_frames(page_num).expect("VirtIO Impl: alloc page failed");

            return (
                paddr.data(),
                NonNull::new(MMArch::phys_2_virt(paddr).unwrap().data() as *mut u8).unwrap(),
            );
        }
    }
    /// @brief 释放用于DMA的内存页
    /// @param paddr 起始物理地址 pages 页数（4k一页）
    /// @return i32 0表示成功
    unsafe fn dma_dealloc(
        paddr: virtio_drivers::PhysAddr,
        _vaddr: NonNull<u8>,
        pages: usize,
    ) -> i32 {
        let page_count = PageFrameCount::new(
            ((pages * PAGE_SIZE + MMArch::PAGE_SIZE - 1) / MMArch::PAGE_SIZE).next_power_of_two(),
        );
        unsafe {
            deallocate_page_frames(PhysPageFrame::new(PhysAddr::new(paddr)), page_count);
        }
        return 0;
    }
    /// @brief mmio物理地址转换为虚拟地址，不需要使用
    /// @param paddr 起始物理地址
    /// @return NonNull<u8> 虚拟地址的指针
    unsafe fn mmio_phys_to_virt(_paddr: virtio_drivers::PhysAddr, _size: usize) -> NonNull<u8> {
        NonNull::new((0) as _).unwrap()
    }
    /// @brief 与真实物理设备共享
    /// @param buffer 要共享的buffer _direction：设备到driver或driver到设备
    /// @return buffer在内存中的物理地址
    unsafe fn share(
        buffer: NonNull<[u8]>,
        _direction: BufferDirection,
    ) -> virtio_drivers::PhysAddr {
        let vaddr = VirtAddr::new(buffer.as_ptr() as *mut u8 as usize);
        //kdebug!("virt:{:x}", vaddr);
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
