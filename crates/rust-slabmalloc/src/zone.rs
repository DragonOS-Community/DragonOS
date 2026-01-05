//! A ZoneAllocator to allocate arbitrary object sizes (up to `ZoneAllocator::MAX_ALLOC_SIZE`)
//!
//! The ZoneAllocator achieves this by having many `SCAllocator`

use crate::*;

/// Creates an instance of a zone, we do this in a macro because we
/// re-use the code in const and non-const functions
///
/// We can get rid of this once the const fn feature is fully stabilized.
macro_rules! new_zone {
    () => {
        ZoneAllocator {
            // TODO(perf): We should probably pick better classes
            // rather than powers-of-two (see SuperMalloc etc.)
            small_slabs: [
                SCAllocator::new(1 << 3),  // 8
                SCAllocator::new(1 << 4),  // 16
                SCAllocator::new(1 << 5),  // 32
                SCAllocator::new(1 << 6),  // 64
                SCAllocator::new(1 << 7),  // 128
                SCAllocator::new(1 << 8),  // 256
                SCAllocator::new(1 << 9),  // 512
                SCAllocator::new(1 << 10), // 1024
                SCAllocator::new(1 << 11), // 2048            ],
            ],
            total: 0,
        }
    };
}

/// A zone allocator for arbitrary sized allocations.
///
/// Has a bunch of `SCAllocator` and through that can serve allocation
/// requests for many different object sizes up to (MAX_SIZE_CLASSES) by selecting
/// the right `SCAllocator` for allocation and deallocation.
///
/// The allocator provides to refill functions `refill` and `refill_large`
/// to provide the underlying `SCAllocator` with more memory in case it runs out.
pub struct ZoneAllocator<'a> {
    small_slabs: [SCAllocator<'a, ObjectPage<'a>>; ZoneAllocator::MAX_BASE_SIZE_CLASSES],
    total: u64,
}

impl<'a> Default for ZoneAllocator<'a> {
    fn default() -> ZoneAllocator<'a> {
        new_zone!()
    }
}

enum Slab {
    Base(usize),
    Unsupported,
}

impl<'a> ZoneAllocator<'a> {
    /// Maximum size which is allocated with ObjectPages (4 KiB pages).
    ///
    /// e.g. this is 4 KiB - 80 bytes of meta-data.
    pub const MAX_BASE_ALLOC_SIZE: usize = 1 << 11;

    /// How many allocators of type SCAllocator<ObjectPage> we have.
    pub const MAX_BASE_SIZE_CLASSES: usize = 9;

    #[cfg(feature = "unstable")]
    pub const fn new() -> ZoneAllocator<'a> {
        new_zone!()
    }

    #[cfg(not(feature = "unstable"))]
    pub fn new() -> ZoneAllocator<'a> {
        new_zone!()
    }

    /// Return maximum size an object of size `current_size` can use.
    ///
    /// Used to optimize `realloc`.
    pub fn get_max_size(current_size: usize) -> Option<usize> {
        match current_size {
            0..=8 => Some(8),
            9..=16 => Some(16),
            17..=32 => Some(32),
            33..=64 => Some(64),
            65..=128 => Some(128),
            129..=256 => Some(256),
            257..=512 => Some(512),
            513..=1024 => Some(1024),
            1025..=2048 => Some(2048),
            _ => None,
        }
    }

    /// Figure out index into zone array to get the correct slab allocator for that size.
    fn get_slab(requested_size: usize) -> Slab {
        match requested_size {
            0..=8 => Slab::Base(0),
            9..=16 => Slab::Base(1),
            17..=32 => Slab::Base(2),
            33..=64 => Slab::Base(3),
            65..=128 => Slab::Base(4),
            129..=256 => Slab::Base(5),
            257..=512 => Slab::Base(6),
            513..=1024 => Slab::Base(7),
            1025..=2048 => Slab::Base(8),
            _ => Slab::Unsupported,
        }
    }

    /// Reclaims empty pages by calling `dealloc` on it and removing it from the
    /// empty lists in the [`SCAllocator`].
    ///
    /// The `dealloc` function is called at most `reclaim_base_max` times for
    /// base pages, and at most `reclaim_large_max` for large pages.
    pub fn try_reclaim_base_pages<F>(&mut self, mut to_reclaim: usize, mut dealloc: F)
    where
        F: Fn(*mut ObjectPage),
    {
        for i in 0..ZoneAllocator::MAX_BASE_SIZE_CLASSES {
            let slab = &mut self.small_slabs[i];
            // reclaim的page数
            let just_reclaimed = slab.try_reclaim_pages(to_reclaim, &mut dealloc);
            self.total -= (just_reclaimed * OBJECT_PAGE_SIZE) as u64;

            to_reclaim = to_reclaim.saturating_sub(just_reclaimed);
            if to_reclaim == 0 {
                break;
            }
        }
    }

    /// 获取scallocator中的还未被分配的空间
    pub fn free_space(&mut self) -> u64 {
        // 记录空闲空间
        let mut free = 0;
        // 遍历所有scallocator
        for count in 0..ZoneAllocator::MAX_BASE_SIZE_CLASSES {
            // 获取scallocator
            let scallocator = &mut self.small_slabs[count];

            // 遍历scallocator中的部分分配的page(partial_page)
            for slab_page in scallocator.slabs.iter_mut() {
                // 剩余可分配object数乘上page中规定的每个object的大小，即空闲空间
                free += slab_page.free_obj_count() * scallocator.size();
            }
            // 遍历scallocator中的empty_page，把空页空间也加上去
            free +=
                scallocator.empty_slabs.elements * (scallocator.obj_per_page * scallocator.size());
        }
        free as u64
    }

    pub fn usage(&mut self) -> SlabUsage {
        let free_num = self.free_space();
        SlabUsage::new(self.total, free_num)
    }
}

unsafe impl<'a> crate::Allocator<'a> for ZoneAllocator<'a> {
    /// Allocate a pointer to a block of memory described by `layout`.
    fn allocate(&mut self, layout: Layout) -> Result<NonNull<u8>, AllocationError> {
        match ZoneAllocator::get_slab(layout.size()) {
            Slab::Base(idx) => self.small_slabs[idx].allocate(layout),
            Slab::Unsupported => Err(AllocationError::InvalidLayout),
        }
    }

    /// Deallocates a pointer to a block of memory, which was
    /// previously allocated by `allocate`.
    ///
    /// # Arguments
    ///  * `ptr` - Address of the memory location to free.
    ///  * `layout` - Memory layout of the block pointed to by `ptr`.
    ///  * `slab_callback` - The callback function to free slab_page in buddy.
    unsafe fn deallocate(
        &mut self,
        ptr: NonNull<u8>,
        layout: Layout,
        slab_callback: &'static dyn CallBack,
    ) -> Result<(), AllocationError> {
        match ZoneAllocator::get_slab(layout.size()) {
            Slab::Base(idx) => {
                let r = self.small_slabs[idx].deallocate(ptr, layout);
                if let Ok(true) = r {
                    self.small_slabs[idx].try_reclaim_pages(
                        1,
                        &mut |slab_page: *mut ObjectPage| {
                            // 将slab_page归还buddy
                            slab_callback
                                .free_slab_page(slab_page as *const _ as *mut u8, ObjectPage::SIZE);
                        },
                    );
                }
                r.map(|_| ())
            }
            Slab::Unsupported => Err(AllocationError::InvalidLayout),
        }
    }

    /// Refills the SCAllocator for a given Layout with an ObjectPage.
    ///
    /// # Safety
    /// ObjectPage needs to be emtpy etc.
    unsafe fn refill(
        &mut self,
        layout: Layout,
        new_page: &'a mut ObjectPage<'a>,
    ) -> Result<(), AllocationError> {
        match ZoneAllocator::get_slab(layout.size()) {
            Slab::Base(idx) => {
                self.small_slabs[idx].refill(new_page);
                // 每refill一个page就为slab的总空间统计加上4KB
                self.total += OBJECT_PAGE_SIZE as u64;
                Ok(())
            }
            Slab::Unsupported => Err(AllocationError::InvalidLayout),
        }
    }
}

/// Slab内存空间使用情况
pub struct SlabUsage {
    // slab总共使用的内存空间
    total: u64,
    // slab的空闲空间
    free: u64,
}

impl SlabUsage {
    /// 初始化SlabUsage
    pub fn new(total: u64, free: u64) -> Self {
        Self { total, free }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn used(&self) -> u64 {
        self.total - self.free
    }

    pub fn free(&self) -> u64 {
        self.free
    }
}
