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
            // 显式 no-retry 上下文（如 CPU buffer 分配）保持原语义：
            // 不触发回收，立即把失败上抛给调用方的回退路径。
            LockedFrameAllocator
                .allocate_below(page_frame_count, crate::mm::PhysAddr::new(usize::MAX))
        } else {
            let allocation = LockedFrameAllocator.allocate(page_frame_count);
            if allocation.is_none() {
                // 对齐 Linux __alloc_pages_slowpath：分配失败先做同步直接回收
                // 再重试，而不是立刻上抛（上抛后用户路径变成 OOM、内核堆路径
                // 变成 alloc_error panic）。回收量按需求量放大，保证重试成功后
                // 留有余量。
                //
                // 这里不能睡眠等待写回：调用方可能持有任意内核锁。回收路径自身
                // 的分配失败由 try_direct_reclaim 的递归守卫挡住，不会重入。
                crate::mm::page::try_direct_reclaim(page_frame_count.data().max(256) * 8, 4, false);
                LockedFrameAllocator.allocate(page_frame_count)
            } else {
                allocation
            }
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

    /// Explicit CPU-buffer first attempt. Buddy allocation never invokes OOM
    /// recovery; small size classes retain the normal slab refill policy.
    /// The caller must use local_dealloc with this same layout.
    pub(crate) unsafe fn alloc_buffer_zeroed(&self, layout: Layout) -> *mut u8 {
        if allocator_select_condition(layout) {
            self.alloc_in_buddy(layout, true)
                .map(|allocation| {
                    let ptr = allocation.as_mut_ptr();
                    core::ptr::write_bytes(ptr, 0, allocation.len());
                    ptr
                })
                .unwrap_or_default()
        } else {
            self.local_alloc_zeroed(layout)
        }
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
                .alloc_in_buddy(layout, false)
                .map(|x| x.as_mut_ptr())
                .unwrap_or_default();
        } else {
            return slab::allocate(layout);
        }
    }

    unsafe fn local_alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if allocator_select_condition(layout) {
            return self
                .alloc_in_buddy(layout, false)
                .map(|x| {
                    let ptr: *mut u8 = x.as_mut_ptr();
                    core::ptr::write_bytes(ptr, 0, x.len());
                    ptr
                })
                .unwrap_or_default();
        } else {
            let ptr = slab::allocate(layout);
            if !ptr.is_null() {
                core::ptr::write_bytes(ptr, 0, layout.size());
            }
            return ptr;
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
        #[cfg(feature = "mm_selftest")]
        if crate::mm::oom::denial::is_current_denied() {
            crate::mm::oom::denial::note_rejection();
            return core::ptr::null_mut();
        }
        let r = self.local_alloc_zeroed(layout);
        if allocator_select_condition(layout) {
            alloc_debug_log(klog_types::LogSource::Buddy, layout, r);
        } else {
            alloc_debug_log(klog_types::LogSource::Slab, layout, r);
        }
        return r;
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        #[cfg(feature = "mm_selftest")]
        if crate::mm::oom::denial::is_current_denied() {
            crate::mm::oom::denial::note_rejection();
            return core::ptr::null_mut();
        }
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
///
/// 到达这里说明 alloc_in_buddy 内的“直接回收后重试”也已经失败。alloc_error_handler
/// 的签名是 `-> !`，无法把内存交还触发分配的调用点；此时也绝不能再 panic：
/// panic 打印路径（error!/println!/Box）自身需要堆分配，堆耗尽时会级联触发
/// 嵌套 panic 直至整机 halt。
///
/// 策略：再回收一次 → 零分配紧急串口输出现场 → 当前线程进入可中断的休眠循环
/// （周期性再回收），把内核与其余 CPU 留给回收线程/OOM 杀进程路径去恢复内存。
#[cfg(target_os = "none")]
#[alloc_error_handler]
pub fn global_alloc_err_handler(layout: Layout) -> ! {
    crate::mm::page::try_direct_reclaim(layout.size().div_ceil(4096).max(256) * 8, 4, false);
    crate::mm::oom::report::report(format_args!(
        "[alloc-exhausted] size={} align={}; parking task, kernel alive\n",
        layout.size(),
        layout.align()
    ));
    loop {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!("sti; hlt", options(nomem, nostack, preserves_flags))
        };
        #[cfg(not(target_arch = "x86_64"))]
        core::hint::spin_loop();
        crate::mm::page::try_direct_reclaim(256, 1, false);
    }
}
