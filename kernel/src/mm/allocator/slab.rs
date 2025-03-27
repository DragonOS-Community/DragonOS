use core::{alloc::Layout, ptr::NonNull, sync::atomic::AtomicBool};

use alloc::boxed::Box;
use log::debug;
use slabmalloc::*;

use crate::{arch::MMArch, mm::MemoryManagementArch, KERNEL_ALLOCATOR};

// 全局slab分配器
pub(crate) static mut SLABALLOCATOR: Option<SlabAllocator> = None;

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

    /// 为对象（2K以内）分配内存空间
    pub(crate) unsafe fn allocate(&mut self, layout: Layout) -> *mut u8 {
        match self.zone.allocate(layout) {
            Ok(nptr) => nptr.as_ptr(),
            Err(AllocationError::OutOfMemory) => {
                let boxed_page = ObjectPage::new();
                assert_eq!(
                    (boxed_page.as_ref() as *const ObjectPage as usize) & (MMArch::PAGE_SIZE - 1),
                    0
                );
                let leaked_page = Box::leak(boxed_page);
                self.zone
                    .refill(layout, leaked_page)
                    .expect("Could not refill?");
                self.zone
                    .allocate(layout)
                    .expect("Should succeed after refill")
                    .as_ptr()
            }
            Err(AllocationError::InvalidLayout) => panic!("Can't allocate this size"),
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

/// 初始化slab分配器
pub unsafe fn slab_init() {
    debug!("trying to init a slab_allocator");
    SLABALLOCATOR = Some(SlabAllocator::new());
    SLABINITSTATE = true.into();
}

pub unsafe fn slab_usage() -> SlabUsage {
    if let Some(ref mut slab) = SLABALLOCATOR {
        slab.zone.usage()
    } else {
        SlabUsage::new(0, 0)
    }
}

/// 归还slab_page给buddy的回调
pub struct SlabCallback;
impl CallBack for SlabCallback {
    unsafe fn free_slab_page(&self, base_addr: *mut u8, size: usize) {
        assert_eq!(base_addr as usize & (MMArch::PAGE_SIZE - 1), 0); // 确认地址4k对齐
        assert_eq!(size, MMArch::PAGE_SIZE); // 确认释放的slab_page大小
        KERNEL_ALLOCATOR.free_in_buddy(base_addr, Layout::from_size_align_unchecked(size, 1));
    }
}
