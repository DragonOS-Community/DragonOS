use crate::include::bindings::bindings::{gfp_t, PAGE_2M_SIZE, kmalloc};
use core::alloc::{GlobalAlloc, Layout};

/// 类kmalloc的分配器应当实现的trait
pub trait LocalAlloc {
    unsafe fn alloc(&mut self, layout: Layout, gfp: gfp_t) -> *mut u8;
    unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout);
}

pub struct KernelAllocator {}

impl LocalAlloc for KernelAllocator {
    unsafe fn alloc(&mut self, layout: Layout, gfp: gfp_t) -> *mut u8 {
        if layout.size() > (PAGE_2M_SIZE as usize / 2) {
            return core::ptr::null_mut();
        }
        return kmalloc(layout.size() as u64, gfp) as *mut u8;
    }
    unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout){
        // todo:
    }
}
