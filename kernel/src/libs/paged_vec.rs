//! Bounded indexed storage whose payload chunks and pointer directory each fit
//! in a fixed small allocation. Reservation is fallible; publication never allocates.
use alloc::{boxed::Box, vec::Vec};
use core::{
    alloc::Layout,
    fmt,
    mem::{size_of, MaybeUninit},
    ops::{Index, IndexMut},
};
use system_error::SystemError;

/// Heap-allocation byte budget, independent of the page-table mapping granule.
/// Supported mapped architectures currently use 4KiB pages; an incomplete MM
/// port must not supply its placeholder granule as a CPU container capacity.
pub(crate) const CHUNK_BYTES: usize = 4096;

pub(crate) struct PagedVec<T, const LIMIT: usize> {
    chunks: Vec<Box<[MaybeUninit<T>]>>,
    len: usize,
}

impl<T, const LIMIT: usize> Default for PagedVec<T, LIMIT> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug, const LIMIT: usize> fmt::Debug for PagedVec<T, LIMIT> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_list().entries(self.iter()).finish()
    }
}

impl<T, const LIMIT: usize> PagedVec<T, LIMIT> {
    pub(crate) const fn new() -> Self {
        Self {
            chunks: Vec::new(),
            len: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn capacity(&self) -> usize {
        if self.chunks.is_empty() {
            0
        } else {
            (self.chunks.len() * (CHUNK_BYTES / size_of::<T>())).min(LIMIT)
        }
    }

    /// Allows bounded consumers to reject unsuitable header types at compile
    /// time as well as retaining the fallible runtime reservation checks.
    pub(crate) const fn layout_supported() -> bool {
        let size = size_of::<T>();
        if size == 0 || size > CHUNK_BYTES || core::mem::align_of::<T>() > CHUNK_BYTES {
            return false;
        }
        let items = CHUNK_BYTES / size;
        LIMIT.div_ceil(items) <= CHUNK_BYTES / size_of::<Box<[MaybeUninit<T>]>>()
    }

    /// Zero-sized and over-page elements are deliberately unsupported. The
    /// directory limit is checked against fat Box pointers, not raw pointers.
    fn layout_limits() -> Result<usize, SystemError> {
        let size = size_of::<T>();
        if size == 0 || size > CHUNK_BYTES {
            return Err(SystemError::EINVAL);
        }
        let items = CHUNK_BYTES / size;
        let payload = Layout::array::<MaybeUninit<T>>(items).map_err(|_| SystemError::EINVAL)?;
        let directory = Layout::array::<Box<[MaybeUninit<T>]>>(LIMIT.div_ceil(items))
            .map_err(|_| SystemError::EINVAL)?;
        if payload.size() > CHUNK_BYTES
            || payload.align() > CHUNK_BYTES
            || directory.size() > CHUNK_BYTES
        {
            return Err(SystemError::EINVAL);
        }
        Ok(items)
    }

    /// Failure may leave additional uninitialized capacity, but never changes
    /// len or existing values. All prepared chunks remain owned by this vector.
    pub(crate) fn try_reserve(&mut self, additional: usize) -> Result<(), SystemError> {
        let required = self
            .len
            .checked_add(additional)
            .ok_or(SystemError::ENOMEM)?;
        if required > LIMIT {
            return Err(SystemError::ENOMEM);
        }
        if additional == 0 {
            return Ok(());
        }
        let items = Self::layout_limits()?;
        let needed = required.div_ceil(items);
        if needed <= self.chunks.len() {
            return Ok(());
        }
        // Exact reservation prevents Vec's geometric growth from producing an
        // over-page directory even when the required directory would fit.
        self.chunks
            .try_reserve_exact(needed - self.chunks.len())
            .map_err(|_| SystemError::ENOMEM)?;
        while self.chunks.len() < needed {
            let count = items.min(LIMIT - self.chunks.len() * items);
            let chunk = Box::<[T]>::try_new_uninit_slice(count).map_err(|_| SystemError::ENOMEM)?;
            self.chunks.push(chunk);
        }
        Ok(())
    }

    /// The caller must reserve first. This method only initializes an owned
    /// slot; it cannot grow either the directory or a payload allocation.
    pub(crate) fn push(&mut self, value: T) {
        assert!(self.len < LIMIT, "paged vector limit exceeded");
        let items = Self::layout_limits().expect("unsupported paged vector layout");
        let chunk = self
            .chunks
            .get_mut(self.len / items)
            .expect("paged vector push without prepared capacity");
        chunk[self.len % items].write(value);
        self.len += 1;
    }

    pub(crate) fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        let items = CHUNK_BYTES / size_of::<T>();
        // SAFETY: exactly 0..old_len was initialized. The removed slot is no
        // longer part of that prefix and will not be read or dropped again.
        Some(unsafe { self.chunks[self.len / items][self.len % items].assume_init_read() })
    }

    pub(crate) fn swap(&mut self, first: usize, second: usize) {
        assert!(first < self.len && second < self.len);
        let items = CHUNK_BYTES / size_of::<T>();
        let first = self.chunks[first / items][first % items].as_mut_ptr();
        let second = self.chunks[second / items][second % items].as_mut_ptr();
        // SAFETY: both slots are initialized, independently allocated chunks
        // never overlap, and ptr::swap also permits identical pointers.
        unsafe { core::ptr::swap(first, second) };
    }

    pub(crate) fn swap_remove(&mut self, index: usize) -> T {
        assert!(index < self.len);
        self.swap(index, self.len - 1);
        self.pop().unwrap()
    }

    /// Requested backing bytes, including spare slots and the fat-pointer
    /// directory. A failed reservation can increase this value without len.
    pub(crate) fn allocated_bytes(&self) -> usize {
        // All chunks except the optional final partial chunk have the same
        // slot count. capacity therefore describes their exact total payload.
        self.chunks.capacity() * size_of::<Box<[MaybeUninit<T>]>>()
            + self.capacity() * size_of::<T>()
    }

    /// Insert into prepared capacity without allocating, preserving order.
    pub(crate) fn insert(&mut self, index: usize, value: T) {
        assert!(index <= self.len);
        self.push(value);
        for position in (index + 1..self.len).rev() {
            self.swap(position - 1, position);
        }
    }

    /// Remove an initialized value, preserving the order of remaining values.
    pub(crate) fn remove(&mut self, index: usize) -> T {
        assert!(index < self.len);
        for position in index + 1..self.len {
            self.swap(position - 1, position);
        }
        self.pop().unwrap()
    }

    pub(crate) fn partition_point(&self, mut predicate: impl FnMut(&T) -> bool) -> usize {
        let (mut left, mut right) = (0, self.len);
        while left < right {
            let middle = left + (right - left) / 2;
            if predicate(&self[middle]) {
                left = middle + 1;
            } else {
                right = middle;
            }
        }
        left
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        (0..self.len).map(|index| &self[index])
    }

    pub(crate) fn last(&self) -> Option<&T> {
        self.len.checked_sub(1).map(|index| &self[index])
    }
}

impl<T, const LIMIT: usize> Index<usize> for PagedVec<T, LIMIT> {
    type Output = T;

    fn index(&self, index: usize) -> &T {
        assert!(index < self.len);
        let items = CHUNK_BYTES / size_of::<T>();
        // SAFETY: only the initialized prefix is indexable.
        unsafe { self.chunks[index / items][index % items].assume_init_ref() }
    }
}

impl<T, const LIMIT: usize> IndexMut<usize> for PagedVec<T, LIMIT> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        assert!(index < self.len);
        let items = CHUNK_BYTES / size_of::<T>();
        // SAFETY: the exclusive vector borrow excludes aliases to this slot.
        unsafe { self.chunks[index / items][index % items].assume_init_mut() }
    }
}

impl<T, const LIMIT: usize> Drop for PagedVec<T, LIMIT> {
    fn drop(&mut self) {
        while self.len != 0 {
            self.len -= 1;
            let items = CHUNK_BYTES / size_of::<T>();
            // SAFETY: shrink the initialized prefix before destroying its last
            // value in place. Do not move a potentially large T onto the stack.
            unsafe {
                self.chunks[self.len / items][self.len % items].assume_init_drop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn chunks_preserve_indices_across_moves_and_reuse() {
        let count = 2 * CHUNK_BYTES / size_of::<usize>() + 3;
        let mut values = PagedVec::<usize, 2047>::new();
        values.try_reserve(count).unwrap();
        for index in 0..count {
            values.push(index);
        }
        assert!(values.iter().copied().eq(0..count));
        assert_eq!(values.last(), Some(&(count - 1)));
        values[1] = 17;
        values.swap(0, count - 1);
        values.swap(1, 1);
        assert_eq!(values.swap_remove(0), count - 1);
        assert_eq!(values[0], 0);
        assert_eq!(values[1], 17);
        assert_eq!(values.pop(), Some(count - 2));
        let previous_chunks = values.chunks.len();
        values.try_reserve(2).unwrap();
        values.push(41);
        values.push(42);
        assert_eq!(values.chunks.len(), previous_chunks);
        assert_eq!(values.pop(), Some(42));
    }

    #[test]
    fn non_copy_values_are_dropped_exactly_once() {
        struct Counted {
            id: usize,
            drops: Arc<Vec<AtomicUsize>>,
        }
        impl Drop for Counted {
            fn drop(&mut self) {
                self.drops[self.id].fetch_add(1, Ordering::Relaxed);
            }
        }
        let drops = Arc::new((0..700).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());
        let mut values = PagedVec::<Counted, 1024>::new();
        values.try_reserve(700).unwrap();
        for id in 0..700 {
            values.push(Counted {
                id,
                drops: drops.clone(),
            });
        }
        drop(values.swap_remove(0));
        drop(values.pop());
        assert_eq!(drops[0].load(Ordering::Relaxed), 1);
        assert_eq!(drops[698].load(Ordering::Relaxed), 1);
        let before = values.len();
        assert_eq!(values.try_reserve(usize::MAX), Err(SystemError::ENOMEM));
        assert_eq!(values.len(), before);
        assert_eq!(values[0].id, 699);
        assert!(values.iter().skip(1).map(|value| value.id).eq(1..698));
        drop(values);
        assert!(drops.iter().all(|count| count.load(Ordering::Relaxed) == 1));
    }

    #[test]
    fn prepared_slots_preserve_element_alignment() {
        #[repr(align(64))]
        struct Aligned(u8);
        let mut values = PagedVec::<Aligned, 70>::new();
        values.try_reserve(70).unwrap();
        for index in 0..70 {
            values.push(Aligned(index));
        }
        for (index, value) in values.iter().enumerate() {
            assert_eq!(value.0 as usize, index);
            assert_eq!(core::ptr::from_ref(value).addr() % 64, 0);
        }
    }

    #[test]
    #[should_panic(expected = "assertion failed: index < self.len")]
    fn uninitialized_reserved_slots_cannot_be_indexed() {
        let mut values = PagedVec::<u8, 8>::new();
        values.try_reserve(8).unwrap();
        let _ = values[0];
    }

    #[test]
    #[should_panic(expected = "paged vector push without prepared capacity")]
    fn push_does_not_implicitly_allocate() {
        PagedVec::<u8, 8>::new().push(1);
    }

    #[test]
    fn ordered_mutations_cross_chunks_and_drop_values_once() {
        let mut values = PagedVec::<Arc<usize>, 1025>::new();
        values.try_reserve(1025).unwrap();
        let marker = Arc::new(4096);
        values.push(marker.clone());
        for index in 0..1024 {
            values.insert(index, Arc::new(index));
        }
        assert_eq!(values.partition_point(|value| **value < 700), 700);
        assert_eq!(**values.last().unwrap(), 4096);
        assert_eq!(Arc::strong_count(&marker), 2);
        let removed = values.remove(1024);
        assert!(Arc::ptr_eq(&removed, &marker));
        assert!(values
            .iter()
            .enumerate()
            .all(|(index, value)| index == **value));
        drop(removed);
        assert_eq!(Arc::strong_count(&marker), 1);
        assert_eq!(
            values.allocated_bytes(),
            values.chunks.capacity() * size_of::<Box<[MaybeUninit<Arc<usize>>]>>()
                + values
                    .chunks
                    .iter()
                    .map(|chunk| core::mem::size_of_val(chunk.as_ref()))
                    .sum::<usize>()
        );
        drop(values);
        assert_eq!(Arc::strong_count(&marker), 1);
    }

    #[test]
    fn limits_and_layouts_are_checked_before_publication() {
        let mut empty = PagedVec::<u8, 0>::new();
        assert!(empty.is_empty());
        assert!(empty.pop().is_none());
        assert!(empty.try_reserve(0).is_ok());
        assert_eq!(empty.try_reserve(1), Err(SystemError::ENOMEM));
        let mut values = PagedVec::<u64, 3>::new();
        values.try_reserve(3).unwrap();
        for value in 0..3 {
            values.push(value);
        }
        assert_eq!(values.try_reserve(1), Err(SystemError::ENOMEM));
        assert!(values.iter().copied().eq(0..3));
        assert_eq!(
            PagedVec::<(), 1>::new().try_reserve(1),
            Err(SystemError::EINVAL)
        );
        #[repr(align(8192))]
        struct TooAligned(u8);
        assert_eq!(
            PagedVec::<TooAligned, 1>::new().try_reserve(1),
            Err(SystemError::EINVAL)
        );
        // Even legal payload elements cannot force an over-page directory.
        assert_eq!(
            PagedVec::<[u8; 1024], 2047>::new().try_reserve(1),
            Err(SystemError::EINVAL)
        );
        let items = PagedVec::<u64, 2047>::layout_limits().unwrap();
        assert!(Layout::array::<MaybeUninit<u64>>(items).unwrap().size() <= CHUNK_BYTES);
        assert!(
            Layout::array::<Box<[MaybeUninit<u64>]>>(2047usize.div_ceil(items))
                .unwrap()
                .size()
                <= CHUNK_BYTES
        );
    }
}

/// Exercise page boundaries and exactly-once destruction using the real
/// container. Compiled only into the existing memory selftest configuration.
#[cfg(feature = "mm_selftest")]
pub(crate) fn selftest() -> bool {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};
    struct Value {
        id: usize,
        drops: Arc<Vec<AtomicUsize>>,
    }
    impl Drop for Value {
        fn drop(&mut self) {
            self.drops[self.id].fetch_add(1, Ordering::Relaxed);
        }
    }
    fn run() -> Result<bool, SystemError> {
        const COUNT: usize = 257;
        let mut counts = Vec::new();
        counts
            .try_reserve_exact(COUNT)
            .map_err(|_| SystemError::ENOMEM)?;
        for _ in 0..COUNT {
            counts.push(AtomicUsize::new(0));
        }
        let drops = Arc::try_new(counts).map_err(|_| SystemError::ENOMEM)?;
        let mut values = PagedVec::<Value, COUNT>::new();
        values.try_reserve(COUNT)?;
        for id in 0..COUNT {
            values.push(Value {
                id,
                drops: drops.clone(),
            });
        }
        if values.try_reserve(1).is_ok()
            || values.try_reserve(usize::MAX).is_ok()
            || values.len() != COUNT
            || values.iter().enumerate().any(|(id, value)| id != value.id)
        {
            return Ok(false);
        }
        values.swap(0, COUNT - 1);
        let removed = values.swap_remove(0);
        if removed.id != COUNT - 1 || values.len() != COUNT - 1 {
            return Ok(false);
        }
        drop(removed);
        drop(values);
        Ok(drops.iter().all(|count| count.load(Ordering::Relaxed) == 1)
            && !PagedVec::<(), 1>::layout_supported()
            && !PagedVec::<[u8; CHUNK_BYTES + 1], 1>::layout_supported())
    }
    run().unwrap_or(false)
}
