//! 这是暴露给C的接口，用于在C语言中使用Rust的内存分配器。

use crate::{
    driver::uart::uart::c_uart_send,
    include::bindings::bindings::{PAGE_KERNEL, PAGE_U_S},
    kdebug,
    libs::align::page_align_up,
};

use super::{
    allocator::page_frame::PageFrameCount,
    kernel_mapper::KernelMapper,
    no_init::pseudo_map_phys,
    page::{PageFlags, PageMapper},
    MemoryManagementArch, PhysAddr, VirtAddr,
};
use crate::mm::MMArch;

/// [EXTERN TO C] Use pseudo mapper to map physical memory to virtual memory.
#[no_mangle]
pub unsafe extern "C" fn rs_pseudo_map_phys(vaddr: usize, paddr: usize, size: usize) {
    let vaddr = VirtAddr::new(vaddr);
    let paddr = PhysAddr::new(paddr);
    let count = PageFrameCount::new(page_align_up(size) / MMArch::PAGE_SIZE);
    pseudo_map_phys(vaddr, paddr, count);
}

/// [EXTERN TO C] Use kernel mapper to map physical memory to virtual memory.
#[no_mangle]
pub unsafe extern "C" fn rs_map_phys(vaddr: usize, paddr: usize, size: usize, flags: usize) {
    let mut vaddr = VirtAddr::new(vaddr);
    let mut paddr = PhysAddr::new(paddr);
    let count = PageFrameCount::new(page_align_up(size) / MMArch::PAGE_SIZE);
    kdebug!("rs_map_phys: vaddr: {vaddr:?}, paddr: {paddr:?}, count: {count:?}, flags: {flags:?}");

    let mut page_flags: PageFlags<MMArch> = PageFlags::new().set_execute(true).set_write(true);
    if flags & PAGE_U_S as usize != 0 {
        page_flags = page_flags.set_user(true);
    }

    let mut kernel_mapper = KernelMapper::lock();

    for _ in 0..count.data() {
        let flusher = kernel_mapper
            .as_mut()
            .unwrap()
            .map_phys(vaddr, paddr, page_flags)
            .unwrap();

        flusher.flush();

        vaddr += MMArch::PAGE_SIZE;
        paddr += MMArch::PAGE_SIZE;
    }
    c_uart_send(0x3f8, 'F' as u8);
}
