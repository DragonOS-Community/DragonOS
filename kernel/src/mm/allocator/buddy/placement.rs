//! Pageblock placement hints. Only complete managed blocks may be retagged;
//! labels describe allocation preference, never migration authorization.
use super::*;
use crate::mm::{allocator::page_frame::FramePlacement, memblock::MemoryAreaAttr};

pub(super) const BLOCK_ORDER: usize = 21;
pub(super) const LOW_ORDERS: usize = BLOCK_ORDER - MIN_ORDER;
pub(super) const MAX_METADATA_HEADROOM: usize = (MAX_ORDER - MIN_ORDER - 1) + LOW_ORDERS;

const BOTH: [FramePlacement; 2] = [FramePlacement::Unmovable, FramePlacement::Movable];
const NEUTRAL: [FramePlacement; 1] = [FramePlacement::Unmovable];

pub(super) fn classes(order: usize) -> &'static [FramePlacement] {
    if order < BLOCK_ORDER {
        &BOTH
    } else {
        &NEUTRAL
    }
}

pub(super) fn allocation_chains(
    requested: u8,
    placement: FramePlacement,
) -> impl Iterator<Item = (u8, FramePlacement)> {
    let other = match placement {
        FramePlacement::Unmovable => FramePlacement::Movable,
        FramePlacement::Movable => FramePlacement::Unmovable,
    };
    (requested as usize..MAX_ORDER)
        .map(move |order| {
            (
                order as u8,
                if order < BLOCK_ORDER {
                    placement
                } else {
                    FramePlacement::Unmovable
                },
            )
        })
        .chain((requested as usize..BLOCK_ORDER).map(move |order| (order as u8, other)))
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct BlockSpan {
    first: usize,
    blocks: usize,
    labels: usize,
}

#[derive(Debug)]
pub(super) struct PlacementMap {
    spans: PhysAddr,
    count: usize,
    labels: PhysAddr,
    blocks: usize,
}

impl PlacementMap {
    /// Old private structural fixtures have no movable consumers. Their
    /// unlisted address ranges retain the conservative unmovable preference.
    pub(super) const fn empty() -> Self {
        Self {
            spans: PhysAddr::new(0),
            count: 0,
            labels: PhysAddr::new(0),
            blocks: 0,
        }
    }

    pub(super) fn normalize<A: MemoryManagementArch>(
        areas: &mut [PhysMemoryArea],
        first_offset: usize,
    ) -> Option<usize> {
        if first_offset & (A::PAGE_SIZE - 1) != 0 {
            return None;
        }
        let mut used = 0;
        for index in 0..areas.len() {
            let area = areas[index];
            if !area.flags.is_empty() {
                return None;
            }
            let end = area.base.data().checked_add(area.size)? & !(A::PAGE_SIZE - 1);
            let mut base = area.base.data().checked_add(A::PAGE_SIZE - 1)? & !(A::PAGE_SIZE - 1);
            if index == 0 {
                base = base.checked_add(first_offset)?;
            }
            if base > end {
                return None;
            }
            if base == end {
                continue;
            }
            areas[used] =
                PhysMemoryArea::new(PhysAddr::new(base), end - base, MemoryAreaAttr::empty());
            used += 1;
        }
        areas[..used].sort_unstable_by_key(|area| area.base);
        let mut merged = 0;
        for index in 0..used {
            let area = areas[index];
            if merged != 0 {
                let previous = &mut areas[merged - 1];
                let previous_end = previous.base.data().checked_add(previous.size)?;
                if previous_end > area.base.data() {
                    return None;
                }
                if previous_end == area.base.data() {
                    previous.size = previous.size.checked_add(area.size)?;
                    continue;
                }
            }
            areas[merged] = area;
            merged += 1;
        }
        Some(merged)
    }

    fn segments(
        areas: &[PhysMemoryArea],
        mut visit: impl FnMut(usize, usize) -> Option<()>,
    ) -> Option<()> {
        let bytes = 1usize << BLOCK_ORDER;
        for area in areas {
            let end = area.base.data().checked_add(area.size)?;
            let first = area.base.data().checked_add(bytes - 1)? / bytes;
            let last = end / bytes;
            if first >= last {
                continue;
            }
            let boundary = (1usize << 32) / bytes;
            if first < boundary && last > boundary {
                visit(first, boundary - first)?;
                visit(boundary, last - boundary)?;
            } else {
                visit(first, last - first)?;
            }
        }
        Some(())
    }

    pub(super) fn requirements(areas: &[PhysMemoryArea]) -> Option<(usize, usize, usize)> {
        let mut count = 0usize;
        let mut blocks = 0usize;
        Self::segments(areas, |_, n| {
            count = count.checked_add(1)?;
            blocks = blocks.checked_add(n)?;
            Some(())
        })?;
        let bytes = count
            .checked_mul(mem::size_of::<BlockSpan>())?
            .checked_add(blocks)?;
        Some((count, blocks, bytes))
    }

    pub(super) unsafe fn initialize<A: MemoryManagementArch>(
        storage: PhysAddr,
        capacity: usize,
        max_spans: usize,
        max_blocks: usize,
        areas: &[PhysMemoryArea],
    ) -> Option<Self> {
        let (count, blocks, bytes) = Self::requirements(areas)?;
        if count > max_spans
            || blocks > max_blocks
            || bytes > capacity
            || !storage.check_aligned(mem::align_of::<BlockSpan>())
        {
            return None;
        }
        let labels = storage + count.checked_mul(mem::size_of::<BlockSpan>())?;
        let virtual_storage = A::phys_2_virt(storage)?;
        core::ptr::write_bytes(virtual_storage.as_ptr::<u8>(), 0, capacity);
        let mut index = 0usize;
        let mut offset = 0usize;
        Self::segments(areas, |first, n| {
            A::write(
                virtual_storage + index * mem::size_of::<BlockSpan>(),
                BlockSpan {
                    first,
                    blocks: n,
                    labels: offset,
                },
            );
            index += 1;
            offset += n;
            Some(())
        })?;
        Some(Self {
            spans: storage,
            count,
            labels,
            blocks,
        })
    }

    fn locate<A: MemoryManagementArch>(&self, block: usize) -> Option<(BlockSpan, usize)> {
        let mut low = 0;
        let mut high = self.count;
        while low < high {
            let mid = low + (high - low) / 2;
            let span: BlockSpan = unsafe {
                A::read(A::phys_2_virt(
                    self.spans + mid * mem::size_of::<BlockSpan>(),
                )?)
            };
            if block < span.first {
                high = mid;
            } else if block - span.first >= span.blocks {
                low = mid + 1;
            } else {
                return Some((span, block - span.first));
            }
        }
        None
    }

    pub(super) fn get<A: MemoryManagementArch>(&self, base: PhysAddr) -> FramePlacement {
        let Some((span, index)) = self.locate::<A>(base.data() >> BLOCK_ORDER) else {
            return FramePlacement::Unmovable;
        };
        let value: u8 = unsafe {
            A::read(
                A::phys_2_virt(self.labels + span.labels + index)
                    .expect("placement label unmapped"),
            )
        };
        match value {
            0 => FramePlacement::Unmovable,
            1 => FramePlacement::Movable,
            _ => panic!("invalid buddy placement"),
        }
    }

    pub(super) fn set<A: MemoryManagementArch>(
        &mut self,
        base: PhysAddr,
        order: usize,
        placement: FramePlacement,
    ) {
        assert!(order >= BLOCK_ORDER && base.check_aligned(1usize << order));
        let count = 1usize << (order - BLOCK_ORDER);
        let Some((span, index)) = self.locate::<A>(base.data() >> BLOCK_ORDER) else {
            // No label means an incomplete edge block, or an old private
            // unmovable-only fixture. Never silently grant movable identity.
            assert_eq!(
                placement,
                FramePlacement::Unmovable,
                "movable run outside managed pageblocks"
            );
            return;
        };
        assert!(index + count <= span.blocks && span.labels + index + count <= self.blocks);
        unsafe {
            core::ptr::write_bytes(
                A::phys_2_virt(self.labels + span.labels + index)
                    .unwrap()
                    .as_ptr::<u8>(),
                placement as u8,
                count,
            );
        }
    }
}

#[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
pub(super) mod selftest;

#[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
pub(super) fn selftest_report() -> alloc::string::String {
    selftest::report()
}

#[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
pub(super) mod map_selftest;
