use klog_types::{AllocLogItem, LogSource};

use crate::{
    arch::mm::LockedFrameAllocator,
    debug::klog::mm::mm_debug_log,
    mm::{MMArch, MemoryManagementArch, VirtAddr},
};

use core::{
    alloc::{AllocError, GlobalAlloc, Layout},
    intrinsics::unlikely,
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

use super::{
    footprint::uses_buddy as allocator_select_condition,
    page_frame::{FrameAllocator, PageFrameCount},
    slab::{self, SLABALLOCATOR},
};

pub(crate) use super::footprint::{allocation_footprint, uses_buddy as allocation_uses_buddy};

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
    let count = super::footprint::buddy_frame_count(layout);
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
    unsafe fn alloc_in_buddy(
        &self,
        layout: Layout,
        no_retry: bool,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let page_frame_count = buddy_frame_count(layout);
        let allocation = if no_retry {
            LockedFrameAllocator
                .allocate_below(page_frame_count, crate::mm::PhysAddr::new(usize::MAX))
        } else {
            LockedFrameAllocator.allocate(page_frame_count)
        };
        let (phy_addr, allocated_frame_count) = allocation.ok_or(AllocError)?;
        if allocated_frame_count != page_frame_count {
            LockedFrameAllocator.free(phy_addr, allocated_frame_count);
            return Err(AllocError);
        }
        debug_assert_eq!(allocated_frame_count, page_frame_count);

        let Some(virt_addr) = (unsafe { MMArch::phys_2_virt(phy_addr) }) else {
            LockedFrameAllocator.free(phy_addr, allocated_frame_count);
            return Err(AllocError);
        };
        if unlikely(virt_addr.is_null()) || !virt_addr.data().is_multiple_of(layout.align()) {
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

    /// Allocate direct-mapped physical storage, without entering GlobalAlloc.
    /// `no_retry` constrains this to one buddy admission and pressure notification;
    /// slab refill follows the same policy and never allocates through GlobalAlloc.
    pub(crate) unsafe fn alloc_physical(
        &self,
        layout: Layout,
        zeroed: bool,
        no_retry: bool,
    ) -> *mut u8 {
        if layout.size() == 0 {
            return layout.align() as *mut u8;
        }
        let pointer = if allocator_select_condition(layout) {
            self.alloc_in_buddy(layout, no_retry)
                .map(|allocation| allocation.as_mut_ptr())
                .unwrap_or_default()
        } else {
            slab::allocate(layout, no_retry)
        };
        if zeroed && !pointer.is_null() {
            core::ptr::write_bytes(pointer, 0, layout.size());
        }
        pointer
    }

    /// Nonrecursive, zeroed metadata storage. Its exact Layout remains compatible
    /// with the direct branch of GlobalAlloc when reconstructed as a Box.
    pub(crate) unsafe fn alloc_metadata(&self, layout: Layout) -> *mut u8 {
        self.alloc_physical(layout, true, true)
    }

    pub(crate) unsafe fn alloc_buffer_zeroed(&self, layout: Layout) -> *mut u8 {
        self.alloc_physical(layout, true, true)
    }

    unsafe fn alloc_global(&self, layout: Layout) -> *mut u8 {
        #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
        if super::heap::HeapPlan::preferred(layout) == super::heap::HeapPlan::Virtual {
            let pointer = crate::mm::vmalloc::try_global_alloc(layout);
            if !pointer.is_null() {
                return pointer;
            }
        }
        // No reclaim, task-local account lookup, or recursive GlobalAlloc here.
        // A constrained caller and a failed virtual attempt get one direct try.
        let pointer = self.alloc_physical(layout, true, true);
        alloc_debug_log(
            if allocator_select_condition(layout) {
                LogSource::Buddy
            } else {
                LogSource::Slab
            },
            layout,
            pointer,
        );
        pointer
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
        self.alloc_physical(layout, false, false)
    }

    unsafe fn local_alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.alloc_physical(layout, true, false)
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
        #[cfg(feature = "mm_selftest")]
        if crate::mm::oom::denial::is_current_denied() {
            crate::mm::oom::denial::note_rejection();
            return core::ptr::null_mut();
        }
        self.alloc_global(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        #[cfg(feature = "mm_selftest")]
        if crate::mm::oom::denial::is_current_denied() {
            crate::mm::oom::denial::note_rejection();
            return core::ptr::null_mut();
        }
        self.alloc_global(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
        if crate::mm::vmalloc::contains(VirtAddr::new(ptr as usize)) {
            crate::mm::vmalloc::global_dealloc(ptr);
            return;
        }
        if allocator_select_condition(layout) {
            dealloc_debug_log(klog_types::LogSource::Buddy, layout, ptr);
        } else {
            dealloc_debug_log(klog_types::LogSource::Slab, layout, ptr);
        }
        self.local_dealloc(ptr, layout);
    }
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

/// 内存分配错误处理函数
#[cfg(target_os = "none")]
#[alloc_error_handler]
pub fn global_alloc_err_handler(layout: Layout) -> ! {
    crate::debug::fatal::allocation_failure(layout)
}
