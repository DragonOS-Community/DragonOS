/// 为virtio-drivers库提供的操作系统接口
use crate::include::bindings::bindings::{
    alloc_pages, free_pages, memory_management_struct, Page, PAGE_2M_SHIFT, PAGE_2M_SIZE,
    PAGE_OFFSET, PAGE_SHARED, ZONE_NORMAL,
};

use crate::mm::virt_2_phys;
use core::mem::size_of;
use core::ptr::NonNull;
use virtio_drivers::{BufferDirection, Hal, PhysAddr, PAGE_SIZE};
pub struct HalImpl;
impl Hal for HalImpl {
    /// @brief 申请用于DMA的内存页
    /// @param pages 页数（4k一页）
    /// @return PhysAddr 获得的内存页的初始物理地址
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        let page_num = (pages * PAGE_SIZE - 1 + PAGE_2M_SIZE as usize) / PAGE_2M_SIZE as usize;
        unsafe {
            let pa = alloc_pages(ZONE_NORMAL, page_num as i32, PAGE_SHARED as u64);
            let page = *pa;
            //kdebug!("alloc pages num:{},Phyaddr={:#x}",pages,page.addr_phys);
            (
                page.addr_phys as PhysAddr,
                NonNull::new((page.addr_phys as PhysAddr + PAGE_OFFSET as usize) as _).unwrap(),
            )
        }
    }
    /// @brief 释放用于DMA的内存页
    /// @param paddr 起始物理地址 pages 页数（4k一页）
    /// @return i32 0表示成功
    fn dma_dealloc(paddr: PhysAddr, _vaddr: NonNull<u8>, pages: usize) -> i32 {
        let page_num = (pages * PAGE_SIZE - 1 + PAGE_2M_SIZE as usize) / PAGE_2M_SIZE as usize;
        unsafe {
            let pa = (memory_management_struct.pages_struct as usize
                + (paddr >> PAGE_2M_SHIFT) * size_of::<Page>()) as *mut Page;
            //kdebug!("free pages num:{},Phyaddr={}",page_num,paddr);
            free_pages(pa, page_num as i32);
        }
        return 0;
    }
    /// @brief mmio物理地址转换为虚拟地址，不需要使用
    /// @param paddr 起始物理地址
    /// @return NonNull<u8> 虚拟地址的指针
    fn mmio_phys_to_virt(_paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        NonNull::new((0) as _).unwrap()
    }
    /// @brief 与真实物理设备共享
    /// @param buffer 要共享的buffer _direction：设备到driver或driver到设备
    /// @return buffer在内存中的物理地址
    fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> PhysAddr {
        let vaddr = buffer.as_ptr() as *mut u8 as usize;
        //kdebug!("virt:{:x}", vaddr);
        // Nothing to do, as the host already has access to all memory.
        virt_2_phys(vaddr)
    }
    /// @brief 停止共享（让主机可以访问全部内存的话什么都不用做）
    fn unshare(_paddr: PhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {
        // Nothing to do, as the host already has access to all memory and we didn't copy the buffer
        // anywhere else.
    }
}
