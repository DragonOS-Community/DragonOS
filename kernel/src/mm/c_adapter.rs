//! 这是暴露给C的接口，用于在C语言中使用Rust的内存分配器。

use core::intrinsics::unlikely;

use alloc::vec::Vec;
use hashbrown::HashMap;

use crate::{
    arch::mm::LowAddressRemapping,
    include::bindings::bindings::{gfp_t, PAGE_U_S},
    kerror,
    libs::{align::page_align_up, spinlock::SpinLock},
    mm::MMArch,
    syscall::SystemError,
};

use super::{
    allocator::page_frame::PageFrameCount, kernel_mapper::KernelMapper, no_init::pseudo_map_phys,
    page::PageFlags, MemoryManagementArch, PhysAddr, VirtAddr,
};

lazy_static! {
    // 用于记录内核分配给C的空间信息
    static ref C_ALLOCATION_MAP: SpinLock<HashMap<VirtAddr, (VirtAddr, usize, usize)>> = SpinLock::new(HashMap::new());
}

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
    // kdebug!("rs_map_phys: vaddr: {vaddr:?}, paddr: {paddr:?}, count: {count:?}, flags: {flags:?}");

    let mut page_flags: PageFlags<MMArch> = PageFlags::new().set_execute(true).set_write(true);
    if flags & PAGE_U_S as usize != 0 {
        page_flags = page_flags.set_user(true);
    }

    let mut kernel_mapper = KernelMapper::lock();
    let mut kernel_mapper = kernel_mapper.as_mut();
    assert!(kernel_mapper.is_some());
    for _i in 0..count.data() {
        let flusher = kernel_mapper
            .as_mut()
            .unwrap()
            .map_phys(vaddr, paddr, page_flags)
            .unwrap();

        flusher.flush();

        vaddr += MMArch::PAGE_SIZE;
        paddr += MMArch::PAGE_SIZE;
    }
}

#[no_mangle]
pub unsafe extern "C" fn kzalloc(size: usize, _gfp: gfp_t) -> usize {
    // kdebug!("kzalloc: size: {size}");
    return do_kmalloc(size, true);
}

#[no_mangle]
pub unsafe extern "C" fn kmalloc(size: usize, _gfp: gfp_t) -> usize {
    // kdebug!("kmalloc: size: {size}");
    // 由于C代码不规范，因此都全部清空
    return do_kmalloc(size, true);
}

fn do_kmalloc(size: usize, _zero: bool) -> usize {
    let space: Vec<u8> = vec![0u8; size];

    assert!(space.len() == size);
    let (ptr, len, cap) = space.into_raw_parts();
    if !ptr.is_null() {
        let vaddr = VirtAddr::new(ptr as usize);
        let len = len as usize;
        let cap = cap as usize;
        let mut guard = C_ALLOCATION_MAP.lock();
        if unlikely(guard.contains_key(&vaddr)) {
            drop(guard);
            unsafe {
                drop(Vec::from_raw_parts(vaddr.data() as *mut u8, len, cap));
            }
            panic!(
                "do_kmalloc: vaddr {:?} already exists in C Allocation Map, query size: {size}, zero: {_zero}",
                vaddr
            );
        }
        // 插入到C Allocation Map中
        guard.insert(vaddr, (vaddr, len, cap));
        return vaddr.data();
    } else {
        return SystemError::ENOMEM.to_posix_errno() as i64 as usize;
    }
}

#[no_mangle]
pub unsafe extern "C" fn kfree(vaddr: usize) -> usize {
    let vaddr = VirtAddr::new(vaddr);
    let mut guard = C_ALLOCATION_MAP.lock();
    let p = guard.remove(&vaddr);
    drop(guard);

    if p.is_none() {
        kerror!("kfree: vaddr {:?} not found in C Allocation Map", vaddr);
        return SystemError::EINVAL.to_posix_errno() as i64 as usize;
    }
    let (vaddr, len, cap) = p.unwrap();
    drop(Vec::from_raw_parts(vaddr.data() as *mut u8, len, cap));
    return 0;
}

#[no_mangle]
pub unsafe extern "C" fn rs_unmap_at_low_addr() -> usize {
    LowAddressRemapping::unmap_at_low_address(true);
    return 0;
}
