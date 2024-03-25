use klog_types::AllocLogItem;

use crate::{
    arch::mm::LockedFrameAllocator,
    debug::klog::mm::mm_debug_log,
    libs::align::page_align_up,
    mm::{MMArch, MemoryManagementArch, VirtAddr},
};

use core::{
    alloc::{AllocError, GlobalAlloc, Layout},
    intrinsics::unlikely,
    ptr::NonNull,
};

use super::page_frame::{FrameAllocator, PageFrameCount};

/// 类kmalloc的分配器应当实现的trait
pub trait LocalAlloc {
    unsafe fn local_alloc(&self, layout: Layout) -> *mut u8;
    unsafe fn local_alloc_zeroed(&self, layout: Layout) -> *mut u8;
    unsafe fn local_dealloc(&self, ptr: *mut u8, layout: Layout);
}

pub struct KernelAllocator;

impl KernelAllocator {
    unsafe fn alloc_in_buddy(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // 计算需要申请的页数，向上取整
        let count = (page_align_up(layout.size()) / MMArch::PAGE_SIZE).next_power_of_two();
        let page_frame_count = PageFrameCount::new(count);
        let (phy_addr, allocated_frame_count) = LockedFrameAllocator
            .allocate(page_frame_count)
            .ok_or(AllocError)?;

        let virt_addr = unsafe { MMArch::phys_2_virt(phy_addr).ok_or(AllocError)? };
        if unlikely(virt_addr.is_null()) {
            return Err(AllocError);
        }

        let slice = unsafe {
            core::slice::from_raw_parts_mut(
                virt_addr.data() as *mut u8,
                allocated_frame_count.data() * MMArch::PAGE_SIZE,
            )
        };
        return Ok(NonNull::from(slice));
    }

    unsafe fn free_in_buddy(&self, ptr: *mut u8, layout: Layout) {
        // 由于buddy分配的页数量是2的幂，因此释放的时候也需要按照2的幂向上取整。
        let count = (page_align_up(layout.size()) / MMArch::PAGE_SIZE).next_power_of_two();
        let page_frame_count = PageFrameCount::new(count);
        let phy_addr = MMArch::virt_2_phys(VirtAddr::new(ptr as usize)).unwrap();
        LockedFrameAllocator.free(phy_addr, page_frame_count);
    }
}

/// 为内核分配器实现LocalAlloc的trait
impl LocalAlloc for KernelAllocator {
    unsafe fn local_alloc(&self, layout: Layout) -> *mut u8 {
        return self
            .alloc_in_buddy(layout)
            .map(|x| x.as_mut_ptr())
            .unwrap_or(core::ptr::null_mut());
    }

    unsafe fn local_alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        return self
            .alloc_in_buddy(layout)
            .map(|x| {
                let ptr: *mut u8 = x.as_mut_ptr();
                core::ptr::write_bytes(ptr, 0, x.len());
                ptr
            })
            .unwrap_or(core::ptr::null_mut());
    }

    unsafe fn local_dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.free_in_buddy(ptr, layout);
    }
}

/// 为内核slab分配器实现GlobalAlloc特性
unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let r = self.local_alloc_zeroed(layout);
        mm_debug_log(
            klog_types::AllocatorLogType::Alloc(AllocLogItem::new(layout, Some(r as usize), None)),
            klog_types::LogSource::Buddy,
        );

        return r;

        // self.local_alloc_zeroed(layout, 0)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let r = self.local_alloc_zeroed(layout);

        mm_debug_log(
            klog_types::AllocatorLogType::AllocZeroed(AllocLogItem::new(
                layout,
                Some(r as usize),
                None,
            )),
            klog_types::LogSource::Buddy,
        );

        return r;
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        mm_debug_log(
            klog_types::AllocatorLogType::Free(AllocLogItem::new(layout, Some(ptr as usize), None)),
            klog_types::LogSource::Buddy,
        );

        self.local_dealloc(ptr, layout);
    }
}

/// 为内核slab分配器实现Allocator特性
// unsafe impl Allocator for KernelAllocator {
//     fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
//         let memory = unsafe {self.local_alloc(layout)};
//         if memory.is_null() {
//             Err(AllocError)
//         } else {
//             let slice = unsafe { core::slice::from_raw_parts_mut(memory, layout.size()) };
//             Ok(unsafe { NonNull::new_unchecked(slice) })
//         }
//     }

//     fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
//         let memory = unsafe {self.local_alloc_zeroed(layout)};
//         if memory.is_null() {
//             Err(AllocError)
//         } else {
//             let slice = unsafe { core::slice::from_raw_parts_mut(memory, layout.size()) };
//             Ok(unsafe { NonNull::new_unchecked(slice) })
//         }
//     }

//     unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
//         self.local_dealloc(ptr.cast().as_ptr(), layout);
//     }
// }

/// 内存分配错误处理函数
#[cfg(target_os = "none")]
#[alloc_error_handler]
pub fn global_alloc_err_handler(layout: Layout) -> ! {
    panic!("global_alloc_error, layout: {:?}", layout);
}
