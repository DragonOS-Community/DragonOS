use core::{alloc::Layout, ptr::NonNull, sync::atomic::AtomicBool};

use alloc::boxed::Box;
use slabmalloc::*;

// 全局slab分配器
pub(crate) static mut SLABALLOCATOR: Option<SlabAllocator> = None;

// slab初始化状态
pub(crate) static mut SLABINITSTATE: AtomicBool = AtomicBool::new(false);

/// slab分配器，实际为一堆小的allocator，可以在里面装4K的page
/// 利用这些allocator可以为对象分配不同大小的空间
pub(crate) struct SlabAllocator {
    zone: ZoneAllocator<'static>,
}

impl SlabAllocator {
    /// 创建slab分配器
    pub fn new() -> SlabAllocator {
        kdebug!("trying to new a slab_allocator");
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
                .deallocate(nptr, layout)
                .expect("Couldn't deallocate");
            return Ok(());
        } else {
            return Ok(());
        }
    }
}

/// 初始化slab分配器
pub unsafe fn slab_init() {
    kdebug!("trying to init a slab_allocator");
    SLABALLOCATOR = Some(SlabAllocator::new());
    SLABINITSTATE = true.into();
}

// 查看slab初始化状态
pub fn slab_init_state() -> bool {
    unsafe { *SLABINITSTATE.get_mut() }
}

pub unsafe fn slab_usage() -> SlabUsage {
    if let Some(ref mut slab) = SLABALLOCATOR {
        slab.zone.usage()
    } else {
        SlabUsage::new(0, 0)
    }
}
