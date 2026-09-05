use bitmap::{traits::BitMapOps, StaticBitmap};
use system_error::SystemError;

/// Linux-compatible SysV IPC id allocator.
///
/// A user-visible IPC id is encoded as `(seq << IPC_ID_SEQ_SHIFT) | idx`.
/// The low bits address the object table, and the high bits distinguish stale
/// userspace ids after an index is reused.
#[derive(Debug)]
pub struct FixedIpcIdAllocator<const CAPACITY: usize, const WORDS: usize> {
    used: StaticBitmap<CAPACITY, WORDS>,
    max_ids: usize,
    next_idx: usize,
    seq: usize,
    last_idx: Option<usize>,
    /// Highest currently allocated index, not the cyclic allocation cursor.
    max_used_idx: Option<usize>,
}

pub type IpcIdAllocator = FixedIpcIdAllocator<32768, { bitmap::static_bitmap_size::<32768>() }>;
pub type ShmIpcIdAllocator = FixedIpcIdAllocator<4096, { bitmap::static_bitmap_size::<4096>() }>;

#[derive(Debug, Clone, Copy)]
pub struct IpcId {
    pub raw: usize,
    pub idx: usize,
    pub seq: usize,
}

impl<const CAPACITY: usize, const WORDS: usize> FixedIpcIdAllocator<CAPACITY, WORDS> {
    pub const IPC_ID_INDEX_BITS: usize = 15;
    pub const IPC_ID_IDX_MASK: usize = (1usize << Self::IPC_ID_INDEX_BITS) - 1;
    pub const IPC_ID_SEQ_SHIFT: usize = Self::IPC_ID_INDEX_BITS;
    pub const IPC_ID_SEQ_MAX: usize = (i32::MAX as usize) >> Self::IPC_ID_SEQ_SHIFT;

    pub fn new(max_ids: usize) -> Result<Self, SystemError> {
        if max_ids == 0
            || max_ids > CAPACITY
            || CAPACITY > Self::IPC_ID_IDX_MASK + 1
            || WORDS != bitmap::static_bitmap_size::<CAPACITY>()
        {
            return Err(SystemError::EINVAL);
        }

        Ok(Self {
            used: StaticBitmap::new(),
            max_ids,
            next_idx: 0,
            seq: 0,
            last_idx: None,
            max_used_idx: None,
        })
    }

    pub fn alloc(&mut self) -> Result<IpcId, SystemError> {
        let idx = self.find_free_idx().ok_or(SystemError::ENOSPC)?;
        let was_used = self.used.set(idx, true);
        debug_assert_eq!(was_used, Some(false));
        self.max_used_idx = Some(self.max_used_idx.map_or(idx, |max| max.max(idx)));
        self.next_idx = if idx + 1 == self.max_ids { 0 } else { idx + 1 };

        if let Some(last_idx) = self.last_idx {
            if idx <= last_idx {
                self.seq += 1;
                if self.seq >= Self::IPC_ID_SEQ_MAX {
                    self.seq = 0;
                }
            }
        }
        self.last_idx = Some(idx);

        Ok(IpcId {
            raw: Self::build_raw(idx, self.seq),
            idx,
            seq: self.seq,
        })
    }

    fn find_free_idx(&self) -> Option<usize> {
        if self.used.get(self.next_idx) == Some(false) {
            return Some(self.next_idx);
        }

        self.used
            .next_false_index(self.next_idx)
            .filter(|&idx| idx < self.max_ids)
            .or_else(|| {
                self.used
                    .first_false_index()
                    .filter(|&idx| idx < self.max_ids)
            })
    }

    pub fn free_idx(&mut self, idx: usize) {
        if idx < self.max_ids {
            self.used.set(idx, false);
            if self.max_used_idx == Some(idx) {
                self.max_used_idx = self.used.prev_index(idx);
            }
        }
    }

    /// Constant-time query; only removing the maximum searches the existing bitmap.
    pub fn max_used_index(&self) -> Option<usize> {
        self.max_used_idx
    }

    pub fn decode(raw: usize) -> Result<IpcId, SystemError> {
        if raw > i32::MAX as usize {
            return Err(SystemError::EINVAL);
        }

        let idx = raw & Self::IPC_ID_IDX_MASK;
        let seq = raw >> Self::IPC_ID_SEQ_SHIFT;
        Ok(IpcId { raw, idx, seq })
    }

    #[inline]
    pub fn build_raw(idx: usize, seq: usize) -> usize {
        (seq << Self::IPC_ID_SEQ_SHIFT) | idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_until_capacity_then_returns_enospc() {
        let mut allocator = IpcIdAllocator::new(2).unwrap();

        assert_eq!(allocator.alloc().unwrap().idx, 0);
        assert_eq!(allocator.alloc().unwrap().idx, 1);
        assert_eq!(allocator.alloc().unwrap_err(), SystemError::ENOSPC);
    }

    #[test]
    fn reuses_freed_index_with_a_new_sequence() {
        let mut allocator = IpcIdAllocator::new(2).unwrap();
        let old = allocator.alloc().unwrap();
        let retained = allocator.alloc().unwrap();
        allocator.free_idx(old.idx);

        let reused = allocator.alloc().unwrap();

        assert_eq!(retained.idx, 1);
        assert_eq!(reused.idx, old.idx);
        assert_ne!(reused.raw, old.raw);
        assert_eq!(reused.seq, old.seq + 1);
    }

    #[test]
    fn rejects_invalid_capacity() {
        assert_eq!(IpcIdAllocator::new(0).unwrap_err(), SystemError::EINVAL);
        assert_eq!(
            IpcIdAllocator::new(IpcIdAllocator::IPC_ID_IDX_MASK + 2).unwrap_err(),
            SystemError::EINVAL
        );

        type InvalidAllocator = FixedIpcIdAllocator<64, 0>;
        assert_eq!(InvalidAllocator::new(64).unwrap_err(), SystemError::EINVAL);
    }

    #[test]
    fn max_used_index_tracks_holes_wraparound_and_empty() {
        let mut allocator = IpcIdAllocator::new(130).unwrap();
        assert_eq!(allocator.max_used_index(), None);
        for idx in 0..130 {
            assert_eq!(allocator.alloc().unwrap().idx, idx);
            assert_eq!(allocator.max_used_index(), Some(idx));
        }
        assert_eq!(allocator.alloc().unwrap_err(), SystemError::ENOSPC);
        assert_eq!(allocator.max_used_index(), Some(129));
        allocator.free_idx(130); // Out-of-range free cannot alter the cache.
        allocator.free_idx(64);
        assert_eq!(allocator.max_used_index(), Some(129));
        for idx in (65..130).rev() {
            allocator.free_idx(idx);
        }
        assert_eq!(allocator.max_used_index(), Some(63));
        // The cyclic allocator reuses the hole across a bitmap word boundary.
        assert_eq!(allocator.alloc().unwrap().idx, 64);
        assert_eq!(allocator.max_used_index(), Some(64));
        for idx in (0..=64).rev() {
            allocator.free_idx(idx);
            assert_eq!(allocator.max_used_index(), idx.checked_sub(1));
        }
        allocator.free_idx(0); // Repeated free is harmless.
        assert_eq!(allocator.max_used_index(), None);
        let id = allocator.alloc().unwrap();
        assert_eq!(allocator.max_used_index(), Some(id.idx));
        allocator.free_idx(id.idx); // Models rollback of a reserved ID.
        assert_eq!(allocator.max_used_index(), None);
    }
}
