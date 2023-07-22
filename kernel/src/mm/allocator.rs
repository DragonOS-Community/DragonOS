use super::gfp::__GFP_ZERO;
use crate::include::bindings::bindings::{gfp_t, kfree, kmalloc, PAGE_2M_SIZE};

use core::alloc::{GlobalAlloc, Layout};

/// 类kmalloc的分配器应当实现的trait
pub trait LocalAlloc {
    unsafe fn local_alloc(&self, layout: Layout, gfp: gfp_t) -> *mut u8;
    unsafe fn local_alloc_zeroed(&self, layout: Layout, gfp: gfp_t) -> *mut u8;
    unsafe fn local_dealloc(&self, ptr: *mut u8, layout: Layout);
}

pub struct KernelAllocator {}

/// 为内核SLAB分配器实现LocalAlloc的trait
impl LocalAlloc for KernelAllocator {
    unsafe fn local_alloc(&self, layout: Layout, gfp: gfp_t) -> *mut u8 {
        if layout.size() > (PAGE_2M_SIZE as usize / 2) {
            return core::ptr::null_mut();
        }
        return kmalloc(layout.size() as u64, gfp) as *mut u8;
    }

    unsafe fn local_alloc_zeroed(&self, layout: Layout, gfp: gfp_t) -> *mut u8 {
        if layout.size() > (PAGE_2M_SIZE as usize / 2) {
            return core::ptr::null_mut();
        }
        return kmalloc(layout.size() as u64, gfp | __GFP_ZERO) as *mut u8;
    }
    #[allow(unused_variables)]
    unsafe fn local_dealloc(&self, ptr: *mut u8, layout: Layout) {
        kfree(ptr as *mut ::core::ffi::c_void);
    }
}

/// 为内核slab分配器实现GlobalAlloc特性
unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.local_alloc(layout, 0)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.local_alloc_zeroed(layout, 0)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.local_dealloc(ptr, layout);
    }
}

/// 内存分配错误处理函数
#[alloc_error_handler]
pub fn global_alloc_err_handler(layout: Layout) -> ! {
    panic!("global_alloc_error, layout: {:?}", layout);
}
