use core::{alloc::Layout, ptr::NonNull, sync::atomic::AtomicBool};

use alloc::boxed::Box;
use log::debug;
use slabmalloc::*;

use crate::libs::spinlock::SpinLock;
use crate::{arch::MMArch, mm::MemoryManagementArch, KERNEL_ALLOCATOR};

use super::kernel_allocator::large_allocation_bytes;

// 全局slab分配器
pub(crate) static SLABALLOCATOR: SpinLock<Option<SlabAllocator>> = SpinLock::new(None);

// slab初始化状态
pub(crate) static mut SLABINITSTATE: AtomicBool = AtomicBool::new(false);

static SLAB_CALLBACK: SlabCallback = SlabCallback;

/// slab分配器，实际为一堆小的allocator，可以在里面装4K的page
/// 利用这些allocator可以为对象分配不同大小的空间
pub(crate) struct SlabAllocator {
    zone: ZoneAllocator<'static>,
}

impl SlabAllocator {
    /// 创建slab分配器
    pub fn new() -> SlabAllocator {
        debug!("trying to new a slab_allocator");
        SlabAllocator {
            zone: ZoneAllocator::new(),
        }
    }

    /// Attempts only the existing size-class storage; never allocates backing pages.
    pub(crate) fn try_allocate(&mut self, layout: Layout) -> Result<NonNull<u8>, AllocationError> {
        self.zone.allocate(layout)
    }

    /// Transfers a prepared buddy page to the size-class lists exactly once.
    /// On rejection, ownership returns to the caller for destruction outside the slab lock.
    unsafe fn refill(
        &mut self,
        layout: Layout,
        page: Box<ObjectPage<'static>>,
    ) -> Result<(), (AllocationError, Box<ObjectPage<'static>>)> {
        assert_eq!(
            (page.as_ref() as *const ObjectPage as usize) & (MMArch::PAGE_SIZE - 1),
            0
        );
        let raw = Box::into_raw(page);
        match self.zone.refill(layout, &mut *raw) {
            Ok(()) => {
                KERNEL_ALLOCATOR.transfer_large_allocation_to_slab(Layout::new::<ObjectPage>());
                Ok(())
            }
            // ZoneAllocator rejects unsupported layouts before linking the page.
            Err(error) => Err((error, Box::from_raw(raw))),
        }
    }

    /// 释放内存空间
    pub(crate) unsafe fn deallocate(
        &mut self,
        ptr: *mut u8,
        layout: Layout,
    ) -> Result<(), AllocationError> {
        if let Some(nptr) = NonNull::new(ptr) {
            self.zone
                .deallocate(nptr, layout, &SLAB_CALLBACK)
                .expect("Couldn't deallocate");
            return Ok(());
        } else {
            return Ok(());
        }
    }
}

/// Allocates backing storage without holding SLABALLOCATOR: buddy OOM retry can
/// wake the reclaim task, whose scheduling must not recursively take this lock.
pub(super) unsafe fn allocate(layout: Layout) -> *mut u8 {
    let first = {
        let mut guard = SLABALLOCATOR.lock_irqsave();
        let Some(slab) = guard.as_mut() else {
            return core::ptr::null_mut();
        };
        slab.try_allocate(layout)
    };
    match first {
        Ok(pointer) => return pointer.as_ptr(),
        Err(AllocationError::InvalidLayout) => panic!("Can't allocate this size"),
        Err(AllocationError::OutOfMemory) => {}
    }

    let mut spare = ObjectPage::try_new().ok();
    let result = {
        let mut guard = SLABALLOCATOR.lock_irqsave();
        match guard.as_mut() {
            None => Err(AllocationError::OutOfMemory),
            Some(slab) => match slab.try_allocate(layout) {
                // Retry even if preparation failed: another CPU may have supplied a slot.
                Err(AllocationError::OutOfMemory) => match spare.take() {
                    None => Err(AllocationError::OutOfMemory),
                    Some(page) => match slab.refill(layout, page) {
                        Ok(()) => slab.try_allocate(layout),
                        Err((error, page)) => {
                            spare = Some(page);
                            Err(error)
                        }
                    },
                },
                result => result,
            },
        }
    };
    // Unused or rejected pages still belong to the buddy/large allocation domain.
    drop(spare);
    match result {
        Ok(pointer) => pointer.as_ptr(),
        Err(AllocationError::OutOfMemory) => core::ptr::null_mut(),
        Err(AllocationError::InvalidLayout) => panic!("Can't allocate this size"),
    }
}

/// 初始化slab分配器
pub unsafe fn slab_init() {
    debug!("trying to init a slab_allocator");
    *SLABALLOCATOR.lock_irqsave() = Some(SlabAllocator::new());
    SLABINITSTATE = true.into();
}

pub fn slab_usage() -> SlabUsage {
    let guard = SLABALLOCATOR.lock_irqsave();
    let size_class_bytes = guard
        .as_ref()
        .map(|slab| slab.zone.usage())
        .unwrap_or_else(|| SlabUsage::new(0))
        .total();
    SlabUsage::new(size_class_bytes + large_allocation_bytes())
}

/// 归还slab_page给buddy的回调
pub struct SlabCallback;
impl CallBack for SlabCallback {
    unsafe fn free_slab_page(&self, base_addr: *mut u8, size: usize) {
        assert_eq!(base_addr as usize & (MMArch::PAGE_SIZE - 1), 0); // 确认地址4k对齐
        assert_eq!(size, MMArch::PAGE_SIZE); // 确认释放的slab_page大小
        KERNEL_ALLOCATOR.free_buddy_raw(base_addr, Layout::from_size_align_unchecked(size, 1));
    }
}
