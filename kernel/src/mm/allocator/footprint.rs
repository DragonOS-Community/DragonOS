//! Allocation slots used by the kernel's size-class and buddy allocators.

use core::alloc::Layout;

use slabmalloc::ZoneAllocator;

use crate::{arch::MMArch, mm::MemoryManagementArch};

#[inline]
pub(crate) fn uses_buddy(layout: Layout) -> bool {
    // Preserve the existing allocator's hot-path comparison and policy.
    layout.size() > 2048
}

/// A valid Layout cannot exceed isize::MAX, so rounding its size to a page
/// power of two still fits usize. Keep allocation and admission on this rule.
pub(super) fn buddy_frame_count(layout: Layout) -> usize {
    layout
        .size()
        .div_ceil(MMArch::PAGE_SIZE)
        .next_power_of_two()
}

/// Bytes reserved for a Box/Vec allocation with this layout.
///
/// Zero-sized owned values have no allocation. This is not the contract of
/// calling the global allocator directly with a zero-sized layout. It reports
/// slot size only, not extra alignment support or slab backing-page overhead.
pub(crate) fn allocation_footprint(layout: Layout) -> u64 {
    if layout.size() == 0 {
        return 0;
    }
    if uses_buddy(layout) {
        (buddy_frame_count(layout) * MMArch::PAGE_SIZE) as u64
    } else {
        ZoneAllocator::get_max_size(layout.size()).expect("small allocator layout has a size class")
            as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footprint_matches_size_classes_and_buddy_runs() {
        for (size, expected) in [
            (0, 0),
            (1, 8),
            (8, 8),
            (9, 16),
            (16, 16),
            (17, 32),
            (33, 64),
            (65, 128),
            (129, 256),
            (257, 512),
            (513, 1024),
            (1025, 2048),
            (2048, 2048),
            (2049, 4096),
            (4096, 4096),
            (4097, 8192),
            (8193, 16384),
        ] {
            let layout = Layout::from_size_align(size, 1).unwrap();
            assert_eq!(allocation_footprint(layout), expected);
            assert_eq!(uses_buddy(layout), size > 2048);
            if uses_buddy(layout) {
                assert_eq!(
                    allocation_footprint(layout),
                    (buddy_frame_count(layout) * MMArch::PAGE_SIZE) as u64
                );
            }
        }
    }

    #[test]
    fn largest_valid_layout_remains_representable() {
        let layout = Layout::from_size_align(isize::MAX as usize, 1).unwrap();
        assert_eq!(allocation_footprint(layout), 1_u64 << (usize::BITS - 1));
    }
}
