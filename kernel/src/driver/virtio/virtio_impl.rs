use crate::include::bindings::bindings::{
    alloc_pages, free_pages, memory_management_struct, Page, PAGE_2M_SHIFT, PAGE_2M_SIZE,
    PAGE_OFFSET, PAGE_SHARED, ZONE_NORMAL,
};

use core::mem::size_of;
use core::ptr::NonNull;
use virtio_drivers::{BufferDirection, Hal, PhysAddr, VirtAddr, PAGE_SIZE};

pub struct HalImpl;
impl Hal for HalImpl {
    /// @brief 申请用于DMA的内存页
    /// @param pages 页数（4k一页）
    /// @return PhysAddr 获得的内存页的初始物理地址
    fn dma_alloc(pages: usize) -> PhysAddr {
        let reminder = pages * PAGE_SIZE % (PAGE_2M_SIZE as usize);
        let page_num = if reminder > 0 {
            (pages * PAGE_SIZE / (PAGE_2M_SIZE as usize) + 1) as i32
        } else {
            (pages * PAGE_SIZE / (PAGE_2M_SIZE as usize)) as i32
        };

        unsafe {
            let pa = alloc_pages(ZONE_NORMAL, page_num, PAGE_SHARED as u64);
            let page = *pa;
            //kdebug!("alloc pages num:{},Phyaddr={}",page_num,page.addr_phys);
            return page.addr_phys as PhysAddr;
        }
    }
    /// @brief 释放用于DMA的内存页
    /// @param paddr 起始物理地址 pages 页数（4k一页）
    /// @return i32 0表示成功
    fn dma_dealloc(paddr: PhysAddr, pages: usize) -> i32 {
        let reminder = pages * PAGE_SIZE % (PAGE_2M_SIZE as usize);
        let page_num = if reminder > 0 {
            (pages * PAGE_SIZE / (PAGE_2M_SIZE as usize) + 1) as i32
        } else {
            (pages * PAGE_SIZE / (PAGE_2M_SIZE as usize)) as i32
        };
        unsafe {
            let pa = (memory_management_struct.pages_struct as usize
                + (paddr >> PAGE_2M_SHIFT) * size_of::<Page>()) as *mut Page;
            //kdebug!("free pages num:{},Phyaddr={}",page_num,paddr);
            free_pages(pa, page_num);
        }
        return 0;
    }
    /// @brief 物理地址转换为虚拟地址
    /// @param paddr 起始物理地址
    /// @return VirtAddr 虚拟地址
    fn phys_to_virt(paddr: PhysAddr) -> VirtAddr {
        paddr + PAGE_OFFSET as usize
    }
    /// @brief 与真实物理设备共享
    /// @param buffer 要共享的buffer _direction：设备到driver或driver到设备
    /// @return buffer在内存中的物理地址
    fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> PhysAddr {
        let vaddr = buffer.as_ptr() as *mut u8 as usize;
        // Nothing to do, as the host already has access to all memory.
        virt_to_phys(vaddr)
    }
    /// @brief 停止共享（让主机可以访问全部内存的话什么都不用做）
    fn unshare(_paddr: PhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {
        // Nothing to do, as the host already has access to all memory and we didn't copy the buffer
        // anywhere else.
    }
}

/// @brief 虚拟地址转换为物理地址
/// @param vaddr 虚拟地址
/// @return PhysAddr 物理地址
fn virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
    vaddr - PAGE_OFFSET as usize
}
