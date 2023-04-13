use crate::mm::allocator::page_frame::FrameAllocator;

pub struct LockedFrameAllocator;

impl FrameAllocator for LockedFrameAllocator {
    unsafe fn allocate(&mut self, count: crate::mm::allocator::page_frame::PageFrameCount) -> Option<crate::mm::PhysAddr> {
        todo!()
    }

    unsafe fn free(&mut self, address: crate::mm::PhysAddr, count: crate::mm::allocator::page_frame::PageFrameCount) {
        todo!()
    }

    unsafe fn usage(&self) -> crate::mm::allocator::page_frame::PageFrameUsage {
        todo!()
    }
}