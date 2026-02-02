use crate::arch::MMArch;
use crate::mm::dma::{dma_alloc_pages_raw, dma_dealloc_pages_raw, DmaAllocOptions, DmaDirection};
use crate::mm::{MemoryManagementArch, PhysAddr, VirtAddr};
use core::ptr::NonNull;
use virtio_drivers::{BufferDirection, Hal};

pub struct HalImpl;
unsafe impl Hal for HalImpl {
    /// @brief 申请用于DMA的内存页
    /// @param pages 页数（4k一页）
    /// @return PhysAddr 获得的内存页的初始物理地址
    fn dma_alloc(
        pages: usize,
        _direction: BufferDirection,
    ) -> (virtio_drivers::PhysAddr, NonNull<u8>) {
        let direction = match _direction {
            BufferDirection::DriverToDevice => DmaDirection::ToDevice,
            BufferDirection::DeviceToDriver => DmaDirection::FromDevice,
            _ => DmaDirection::Bidirectional,
        };
        let options = DmaAllocOptions {
            direction,
            use_pool: false,
            ..Default::default()
        };
        dma_alloc_pages_raw(pages, options)
    }
    /// @brief 释放用于DMA的内存页
    /// @param paddr 起始物理地址 pages 页数（4k一页）
    /// @return i32 0表示成功
    unsafe fn dma_dealloc(
        paddr: virtio_drivers::PhysAddr,
        vaddr: NonNull<u8>,
        pages: usize,
    ) -> i32 {
        dma_dealloc_pages_raw(paddr, vaddr, pages)
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
