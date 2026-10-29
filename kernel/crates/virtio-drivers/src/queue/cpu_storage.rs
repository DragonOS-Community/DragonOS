//! CPU-only indexed queue state. Neither the page directory nor a data block
//! requires a high-order allocation; no contiguous virtual address is needed.

use crate::{Error, Result, PAGE_SIZE};
use core::{
    alloc::Layout,
    mem::{align_of, forget, size_of},
    ops::{Index, IndexMut},
    ptr::NonNull,
};

/// Shared by the page owners and the small boxed queue shell. Restrict every
/// CPU allocation to one page, including directories and unusual element types.
pub(super) fn try_allocate(layout: Layout) -> Result<NonNull<u8>> {
    if layout.size() == 0 || layout.size() > PAGE_SIZE || layout.align() > PAGE_SIZE {
        return Err(Error::InvalidParam);
    }
    #[cfg(test)]
    tests::before_allocate(layout)?;
    // SAFETY: the checked layout is nonempty. Null is propagated, never used.
    NonNull::new(unsafe { alloc::alloc::alloc(layout) }).ok_or(Error::MemoryError)
}

/// Owns a fully initialized block of Copy values. No element destructor can
/// allocate, block, or make partially constructed queue rollback recursive.
#[derive(Debug)]
struct Block<T: Copy> {
    pointer: NonNull<T>,
    len: usize,
}

impl<T: Copy> Block<T> {
    fn try_filled(len: usize, value: T) -> Result<Self> {
        let layout = Layout::array::<T>(len).map_err(|_| Error::InvalidParam)?;
        let pointer = try_allocate(layout)?.cast::<T>();
        for index in 0..len {
            // SAFETY: allocation covers len T values with T alignment; Copy
            // initialization has no user-provided callback or panic boundary.
            unsafe { pointer.as_ptr().add(index).write(value) };
        }
        #[cfg(test)]
        tests::block_created();
        Ok(Self { pointer, len })
    }

    fn into_raw(self) -> NonNull<T> {
        let pointer = self.pointer;
        forget(self);
        pointer
    }
}

impl<T: Copy> Drop for Block<T> {
    fn drop(&mut self) {
        let layout = Layout::array::<T>(self.len).expect("owned block layout");
        // SAFETY: pointer and len are the original nonempty allocation. Copy
        // elements have no destructor; ownership has not escaped this owner.
        unsafe { alloc::alloc::dealloc(self.pointer.as_ptr().cast(), layout) };
        #[cfg(test)]
        tests::block_destroyed();
    }
}

#[derive(Debug)]
pub(super) struct PagedArray<T: Copy> {
    directory: Option<Block<NonNull<T>>>,
    initialized: usize,
    len: usize,
    per_block: usize,
}

impl<T: Copy> PagedArray<T> {
    pub(super) fn try_filled(len: usize, value: T) -> Result<Self> {
        let element_size = size_of::<T>();
        if element_size == 0 || element_size > PAGE_SIZE || align_of::<T>() > PAGE_SIZE {
            return Err(Error::InvalidParam);
        }
        // Check the logical length independently of allocation chunking.
        Layout::array::<T>(len).map_err(|_| Error::InvalidParam)?;
        let per_block = PAGE_SIZE / element_size;
        let blocks = len.checked_add(per_block - 1).ok_or(Error::InvalidParam)? / per_block;
        let mut array = Self {
            directory: if blocks == 0 {
                None
            } else {
                Some(Block::try_filled(blocks, NonNull::dangling())?)
            },
            initialized: 0,
            len,
            per_block,
        };
        while array.initialized < blocks {
            let count = (len - array.initialized * per_block).min(per_block);
            let block = Block::try_filled(count, value)?;
            // SAFETY: initialized is within the fully initialized directory.
            // Transfer the new block before publishing its ownership count.
            unsafe {
                array
                    .directory
                    .as_mut()
                    .unwrap()
                    .pointer
                    .as_ptr()
                    .add(array.initialized)
                    .write(block.into_raw());
            }
            array.initialized += 1;
        }
        Ok(array)
    }

    fn element(&self, index: usize) -> *mut T {
        assert!(index < self.len, "queue metadata index out of bounds");
        // SAFETY: construction initialized every directory entry and block;
        // index bounds guarantee the last, possibly short block is respected.
        unsafe {
            self.directory
                .as_ref()
                .unwrap()
                .pointer
                .as_ptr()
                .add(index / self.per_block)
                .read()
                .as_ptr()
                .add(index % self.per_block)
        }
    }

    #[cfg(test)]
    pub(super) fn iter(&self) -> impl Iterator<Item = &T> {
        (0..self.len).map(move |index| &self[index])
    }
}

impl<T: Copy> Index<usize> for PagedArray<T> {
    type Output = T;
    fn index(&self, index: usize) -> &T {
        // SAFETY: the initialized element is borrowed through the sole owner.
        unsafe { &*self.element(index) }
    }
}

impl<T: Copy> IndexMut<usize> for PagedArray<T> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        // SAFETY: &mut self excludes any other reference to these CPU bytes.
        unsafe { &mut *self.element(index) }
    }
}

impl<T: Copy> Drop for PagedArray<T> {
    fn drop(&mut self) {
        for index in 0..self.initialized {
            // SAFETY: only initialized entries own a block, each exactly once.
            let pointer = unsafe {
                self.directory
                    .as_ref()
                    .unwrap()
                    .pointer
                    .as_ptr()
                    .add(index)
                    .read()
            };
            drop(Block {
                pointer,
                len: (self.len - index * self.per_block).min(self.per_block),
            });
        }
        // The directory owner is freed after all its data blocks.
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use core::cell::Cell;

    std::thread_local! {
        // fail-at, attempts, live blocks, maximum request size. No global test
        // interference or allocator calls from inside this bookkeeping.
        static STATE: Cell<(usize, usize, usize, usize)> = const { Cell::new((0, 0, 0, 0)) };
    }

    pub(crate) fn reset(fail_at: usize) {
        STATE.with(|state| {
            assert_eq!(state.get().2, 0, "previous CPU metadata leaked");
            state.set((fail_at, 0, 0, 0));
        });
    }
    pub(crate) fn snapshot() -> (usize, usize, usize, usize) {
        STATE.with(Cell::get)
    }
    pub(super) fn before_allocate(layout: Layout) -> Result<()> {
        STATE.with(|state| {
            let (fail_at, attempts, live, max) = state.get();
            state.set((fail_at, attempts + 1, live, max.max(layout.size())));
            if attempts + 1 == fail_at {
                Err(Error::MemoryError)
            } else {
                Ok(())
            }
        })
    }
    pub(super) fn block_created() {
        STATE.with(|state| {
            let (fail, attempts, live, max) = state.get();
            state.set((fail, attempts, live + 1, max));
        });
    }
    pub(super) fn block_destroyed() {
        STATE.with(|state| {
            let (fail, attempts, live, max) = state.get();
            state.set((fail, attempts, live.checked_sub(1).unwrap(), max));
        });
    }

    #[test]
    fn blocks_are_bounded_and_failure_releases_every_predecessor() {
        for fail in 0..=6 {
            reset(fail);
            let array = PagedArray::try_filled(1025, 17u64);
            // 1 directory + 3 data blocks.
            if (1..=4).contains(&fail) {
                assert!(matches!(array, Err(Error::MemoryError)));
            } else {
                let mut array = array.unwrap();
                for index in [0, 511, 512, 1023, 1024] {
                    assert_eq!(array[index], 17);
                    array[index] = index as u64;
                }
            }
            assert_eq!(snapshot().2, 0);
            assert!(snapshot().3 <= PAGE_SIZE);
        }
        reset(0);
    }

    #[test]
    fn rejects_unsupported_elements_and_oversized_directory() {
        #[derive(Clone, Copy)]
        #[repr(align(8192))]
        struct OverAligned;
        assert!(matches!(
            PagedArray::try_filled(1, ()),
            Err(Error::InvalidParam)
        ));
        assert!(matches!(
            PagedArray::try_filled(1, OverAligned),
            Err(Error::InvalidParam)
        ));
        assert!(matches!(
            PagedArray::try_filled(1, [0u8; PAGE_SIZE + 1]),
            Err(Error::InvalidParam)
        ));
        assert!(matches!(
            PagedArray::try_filled(usize::MAX, 0u8),
            Err(Error::InvalidParam)
        ));
        // Each value occupies one whole block; its pointer directory exceeds PAGE.
        assert!(matches!(
            PagedArray::try_filled(PAGE_SIZE / size_of::<usize>() + 1, [0u8; PAGE_SIZE]),
            Err(Error::InvalidParam)
        ));
    }
}
