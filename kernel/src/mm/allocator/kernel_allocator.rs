use klog_types::{AllocLogItem, LogSource};

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
    sync::atomic::{AtomicU64, Ordering},
};

use super::{
    page_frame::{FrameAllocator, PageFrameCount},
    slab::SLABALLOCATOR,
};

/// 类kmalloc的分配器应当实现的trait
pub trait LocalAlloc {
    #[allow(dead_code)]
    unsafe fn local_alloc(&self, layout: Layout) -> *mut u8;
    unsafe fn local_alloc_zeroed(&self, layout: Layout) -> *mut u8;
    unsafe fn local_dealloc(&self, ptr: *mut u8, layout: Layout);
}

pub struct KernelAllocator;

/// Bytes currently owned by kernel heap allocations that bypass the size-class
/// allocator and are backed directly by buddy pages.
static LARGE_ALLOCATION_BYTES: AtomicU64 = AtomicU64::new(0);

#[inline]
fn buddy_frame_count(layout: Layout) -> PageFrameCount {
    let count = (page_align_up(layout.size()) / MMArch::PAGE_SIZE).next_power_of_two();
    PageFrameCount::new(count)
}

#[inline]
fn debit_large_allocation(bytes: u64) {
    LARGE_ALLOCATION_BYTES
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_sub(bytes)
        })
        .expect("large kernel allocation accounting underflow");
}

pub(super) fn large_allocation_bytes() -> u64 {
    LARGE_ALLOCATION_BYTES.load(Ordering::Relaxed)
}

impl KernelAllocator {
    unsafe fn alloc_in_buddy(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let page_frame_count = buddy_frame_count(layout);
        let (phy_addr, allocated_frame_count) = LockedFrameAllocator
            .allocate(page_frame_count)
            .ok_or(AllocError)?;
        if allocated_frame_count != page_frame_count {
            LockedFrameAllocator.free(phy_addr, allocated_frame_count);
            return Err(AllocError);
        }
        debug_assert_eq!(allocated_frame_count, page_frame_count);

        let Some(virt_addr) = (unsafe { MMArch::phys_2_virt(phy_addr) }) else {
            LockedFrameAllocator.free(phy_addr, allocated_frame_count);
            return Err(AllocError);
        };
        if unlikely(virt_addr.is_null()) {
            LockedFrameAllocator.free(phy_addr, allocated_frame_count);
            return Err(AllocError);
        }

        let allocated_bytes = allocated_frame_count.bytes();
        let slice = unsafe {
            core::slice::from_raw_parts_mut(virt_addr.data() as *mut u8, allocated_bytes)
        };
        LARGE_ALLOCATION_BYTES
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(allocated_bytes as u64)
            })
            .expect("large kernel allocation accounting overflow");
        return Ok(NonNull::from(slice));
    }

    /// Transfers a direct buddy allocation into the size-class slab domain.
    /// The backing pages remain allocated; only their accounting owner changes.
    pub(super) fn transfer_large_allocation_to_slab(&self, layout: Layout) {
        debug_assert!(allocator_select_condition(layout));
        debit_large_allocation(buddy_frame_count(layout).bytes() as u64);
    }

    /// Frees buddy pages whose accounting is owned by another allocator.
    pub(super) unsafe fn free_buddy_raw(&self, ptr: *mut u8, layout: Layout) {
        let page_frame_count = buddy_frame_count(layout);
        let phy_addr = MMArch::virt_2_phys(VirtAddr::new(ptr as usize)).unwrap();
        LockedFrameAllocator.free(phy_addr, page_frame_count);
    }

    unsafe fn dealloc_large(&self, ptr: *mut u8, layout: Layout) {
        let page_frame_count = buddy_frame_count(layout);
        // Validate the address before changing accounting. A failed conversion
        // must not leave a live allocation unaccounted.
        let phy_addr = MMArch::virt_2_phys(VirtAddr::new(ptr as usize)).unwrap();
        debit_large_allocation(page_frame_count.bytes() as u64);
        LockedFrameAllocator.free(phy_addr, page_frame_count);
    }
}

/// 为内核分配器实现LocalAlloc的trait
impl LocalAlloc for KernelAllocator {
    unsafe fn local_alloc(&self, layout: Layout) -> *mut u8 {
        if allocator_select_condition(layout) {
            return self
                .alloc_in_buddy(layout)
                .map(|x| x.as_mut_ptr())
                .unwrap_or_default();
        } else {
            let mut guard = SLABALLOCATOR.lock_irqsave();
            if let Some(ref mut slab) = *guard {
                return slab.allocate(layout);
            }
            return core::ptr::null_mut();
        }
    }

    unsafe fn local_alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if allocator_select_condition(layout) {
            return self
                .alloc_in_buddy(layout)
                .map(|x| {
                    let ptr: *mut u8 = x.as_mut_ptr();
                    core::ptr::write_bytes(ptr, 0, x.len());
                    ptr
                })
                .unwrap_or_default();
        } else {
            let mut guard = SLABALLOCATOR.lock_irqsave();
            if let Some(ref mut slab) = *guard {
                let ptr = slab.allocate(layout);
                drop(guard);
                if !ptr.is_null() {
                    core::ptr::write_bytes(ptr, 0, layout.size());
                }
                return ptr;
            }
            return core::ptr::null_mut();
        }
    }

    unsafe fn local_dealloc(&self, ptr: *mut u8, layout: Layout) {
        if allocator_select_condition(layout) {
            self.dealloc_large(ptr, layout)
        } else {
            let mut guard = SLABALLOCATOR.lock_irqsave();
            if let Some(ref mut slab) = *guard {
                slab.deallocate(ptr, layout).unwrap()
            }
        }
    }
}

/// 为内核slab分配器实现GlobalAlloc特性
unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let r = self.local_alloc_zeroed(layout);
        if allocator_select_condition(layout) {
            alloc_debug_log(klog_types::LogSource::Buddy, layout, r);
        } else {
            alloc_debug_log(klog_types::LogSource::Slab, layout, r);
        }
        return r;
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let r = self.local_alloc_zeroed(layout);
        if allocator_select_condition(layout) {
            alloc_debug_log(klog_types::LogSource::Buddy, layout, r);
        } else {
            alloc_debug_log(klog_types::LogSource::Slab, layout, r);
        }
        return r;
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if allocator_select_condition(layout) || (ptr as usize).is_multiple_of(4096) {
            dealloc_debug_log(klog_types::LogSource::Buddy, layout, ptr);
        } else {
            dealloc_debug_log(klog_types::LogSource::Slab, layout, ptr);
        }
        self.local_dealloc(ptr, layout);
    }
}

/// 判断选择buddy分配器还是slab分配器
fn allocator_select_condition(layout: Layout) -> bool {
    layout.size() > 2048
}

fn alloc_debug_log(source: LogSource, layout: Layout, ptr: *mut u8) {
    mm_debug_log(
        klog_types::AllocatorLogType::Alloc(AllocLogItem::new(layout, Some(ptr as usize), None)),
        source,
    )
}

fn dealloc_debug_log(source: LogSource, layout: Layout, ptr: *mut u8) {
    mm_debug_log(
        klog_types::AllocatorLogType::Free(AllocLogItem::new(layout, Some(ptr as usize), None)),
        source,
    )
}

// 为内核slab分配器实现Allocator特性
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
